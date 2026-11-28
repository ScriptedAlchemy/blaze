#!/usr/bin/env python3
"""distv2.py: prove distributed compilation and cache reuse with real export clients.

Input, one JSON value (`python3 harness/distv2.py '{...}'`):
    blaze        the blaze binary under test
    dist_local   the `blaze-dist-local` binary (coordinator)
    serve        the `blaze-serve` binary (cache server)
    scratch      a new directory for homes, stores, sockets and retained evidence
    project      (optional) the workspace to build; default harness/distv2/fixture
    args         (optional) additional arguments of `blaze export`
    program      (optional) the path of the program, relative to its build directory's debug/;
                 default "app"
    run          (optional) the arguments it is run with; default ["11"]
    faults       (optional) run the fault cases; default true
    release_fold (optional) require two real holders for both LLVM and folded work; default false
    cache_overlap (optional) run only the independent native-cache overlap oracle; default false
    dev_latency  (optional) measure warm dev exports separately from the fault run; default false
    repetitions  (optional) independent dev latency samples per mode; default 2

    profile      (optional) the directory under the target directory programs are in; default "debug"
    timeout      (optional) bound in seconds for each build/fault stage; default 3600
    command_timeout (optional) seconds for probes, stop and running the program; default 60
    env          (optional) explicit toolchain/runtime environment for every child
    module_builder (optional) explicit D52 bootstrap proc_macro_modules executable for fixture modules;
                 runs sandboxed before export, never invoked by blaze
    cores        (optional) per-process core budget; otherwise preserve the caller's BLAZE_CORES.
                 Dev latency requires an explicit total budget of at least two Linux CPUs.
    stall_bound, daemon_memory_gb (optional) legacy inputs retained in provenance only

Checks, each on what the build itself reports (BLAZE_DIST_REPORT) and on running the program:
    solo         one export without a coordinator: the program the others are held to
    two workers  two processes export at once: every unit done once by agreement, both compile, both
                 programs behave as solo's; granted templates are not recompiled in the tail
                 or by another worker, and raw emitted keys are all published
    cold cache   a third process with an empty store takes published units and compiles none
    warm         a build with nothing changed reports no execution
    edit         one edited function: only what it reaches is worked again; for dev, a
                 warm primary consumes the final reachability published by the helper
    other PR     another checkout, in a pull request's namespace, with one crate edited: the
                 units it shares with what was published come from the cache, and the edited
                 ones compile; a newer revision of that pull request supersedes an older one
    worker lost  a worker killed in the middle: the other finishes alone, same program
    fenced       a worker stopped past its lease and resumed: its work is taken back, it cannot
                 commit after that, and both end with the program of a build alone
    no carrier   with no coordinator the build stops naming it; `fallback` compiles here
    bad cache    a committed pack damaged: both `never` and `fallback` refuse it, without recompilation

Every program is run: what it prints covers a panic unwinding across units, a static of another
crate, and a build script's output.

A separate input with role="controller" runs the three-host proof. It names the sealed
compiler and services, the local Unix socket and loopback cache address, and two pinned
SSH helper aliases with forwards. The controller launches role="primary" and role="helper"
sessions that each run a real export against one coordinator. The retained three-host wall
times include the explicit correctness barrier.
"""
from __future__ import annotations

import dataclasses
import hashlib
import json
import os
import re
import shlex
import shutil
import signal
import socket
import struct
import subprocess
import sys
import time
from contextlib import closing, contextmanager
from http.client import HTTPConnection
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from selectors import EVENT_READ, DefaultSelector
from threading import Event, Lock, Thread
from uuid import UUID

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
    cache_overlap: bool = False
    dev_latency: bool = False
    repetitions: int = 2
    profile: str = "debug"
    release_fold: bool = False
    stall_bound: int | None = None
    timeout: int = 3600
    command_timeout: int = 60
    env: dict[str, str] = dataclasses.field(default_factory=dict)
    cores: int | None = None
    daemon_memory_gb: int | None = None
    module_builder: str | None = None


def read_input(text: str) -> dict:
    raw = json.loads(sys.stdin.read() if text == "-" else (Path(text).read_text() if not text.lstrip().startswith(("{", "[")) else text))
    if not isinstance(raw, dict):
        raise Failed("input must be a JSON object")
    return raw


def load(text: str) -> Input:
    return load_profile(read_input(text))


def load_profile(raw: dict) -> Input:
    for name in ("args", "run"):
        if name in raw:
            if not isinstance(raw[name], list) or not all(isinstance(value, str) and "\0" not in value for value in raw[name]):
                raise Failed(f"{name} must be a JSON array of strings without NUL bytes")
            raw[name] = tuple(raw[name])
    given = Input(**raw)
    if given.module_builder is not None:
        if not isinstance(given.module_builder, str) or not given.module_builder or "\0" in given.module_builder:
            raise Failed("module_builder must name a nonempty executable path without NUL bytes")
        builder = Path(given.module_builder).resolve()
        if not builder.is_file() or not os.access(builder, os.X_OK):
            raise Failed(f"module_builder is not an executable file: {builder}")
        given = dataclasses.replace(given, module_builder=str(builder))
    for name in ("blaze", "dist_local", "serve", "scratch", "project"):
        value = getattr(given, name)
        if not isinstance(value, str) or not value or "\0" in value:
            raise Failed(f"{name} must name a nonempty path without NUL bytes")
    for name in ("timeout", "command_timeout", "cores", "daemon_memory_gb", "stall_bound", "repetitions"):
        value = getattr(given, name)
        if value is None and name in ("cores", "daemon_memory_gb", "stall_bound"):
            continue
        if type(value) is not int or value <= 0:
            raise Failed(f"{name} must be a positive integer when supplied")
    if not isinstance(given.env, dict) or not all(isinstance(k, str) and isinstance(v, str) for k, v in given.env.items()):
        raise Failed("env must map strings to strings")
    if any(name.startswith("BLAZE_DIST_") for name in given.env):
        raise Failed("env must not override BLAZE_DIST_*: distribution and policies are controlled case inputs")
    for name in ("faults", "release_fold", "cache_overlap", "dev_latency"):
        if type(getattr(given, name)) is not bool:
            raise Failed(f"{name} must be boolean")
    if given.release_fold and ("--release" not in given.args or given.profile != "release"):
        raise Failed('release_fold requires args ["--release"] and profile "release"')
    if given.dev_latency and (given.cache_overlap or given.profile != "debug" or given.args
                              or given.cores is None or given.cores < 2):
        raise Failed("dev_latency requires debug exports with no additional args, no cache_overlap and at least two cores")
    for name in ("profile", "program"):
        value = getattr(given, name)
        if (not isinstance(value, str) or not value or "\0" in value
                or any(part in ("", ".", "..") for part in value.split("/"))
                or Path(value).is_absolute()):
            raise Failed(f"{name} must be a relative path below the build target")
    return given


def check(ok: bool, what: str) -> None:
    print(("ok   " if ok else "FAIL ") + what, flush=True)
    if not ok:
        raise Failed(what)


def require_build(code: int, output: str, phase: str, log: Path) -> None:
    """Keep the first task failure as well as the final diagnostic in the CI summary."""
    if code == 0:
        check(True, phase)
        return
    limit = 4096
    excerpt = output.strip()
    if len(excerpt) > 2 * limit:
        excerpt = excerpt[:limit] + "\n... (see full build log) ...\n" + excerpt[-limit:]
    raise Failed(f"{phase}: exit {code}; full log: {log}\n{excerpt}")


def free_port() -> int:
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


def host_triple(blaze: str, env: dict, timeout: int, evidence_dir: Path) -> str:
    """Record the selected compiler's target before starting the coordinator."""
    argv = [blaze, "toolchain", "--json"]
    out = subprocess.run(argv, env=env, capture_output=True, text=True,
                         check=False, timeout=timeout, stdin=subprocess.DEVNULL)
    (evidence_dir / "worker-protocol-probe.json").write_text(json.dumps({
        "argv": argv, "returncode": out.returncode, "stdout": out.stdout, "stderr": out.stderr,
    }, indent=2) + "\n")
    try:
        reply = json.loads(out.stdout)
        host = reply.get("host") if isinstance(reply, dict) else None
    except json.JSONDecodeError:
        host = None
    if out.returncode != 0 or not isinstance(host, str) or not host or any(c.isspace() for c in host):
        raise Failed(
            "`blaze toolchain --json` must return a host before distributed exports can run. "
            f"Probe exit {out.returncode}; see {evidence_dir / 'worker-protocol-probe.json'}")
    return host


def source_state(repository: Path, timeout: int) -> tuple[str, str, bytes]:
    """Bind acceptance to HEAD, its tracked patch, and the absence of untracked source."""
    def git(*args: str) -> bytes:
        return subprocess.check_output(["git", "-C", str(repository), *args], timeout=timeout)

    commit = git("rev-parse", "HEAD").decode().strip()
    tree = git("rev-parse", "HEAD^{tree}").decode().strip()
    # git diff HEAD includes staged and unstaged tracked changes but not untracked source.
    # An untracked module could alter a compiled binary without changing that patch.
    untracked = [os.fsdecode(name) for name in git("ls-files", "--others", "--exclude-standard", "-z").split(b"\0") if name]
    if untracked:
        raise Failed(f"untracked source is absent from the provenance patch: {untracked[:8]} ({len(untracked)} files)")
    return commit, tree, git("diff", "HEAD", "--binary")


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
        self.services: list[subprocess.Popen] = []
        self.clients: list[tuple[subprocess.Popen, float, str]] = []
        self.prepared_modules: set[str] = set()
        self.builder_hash = file_hash(Path(given.module_builder)) if given.module_builder else None
        self.suspended: set[int] = set()
        self.result_gates: list[Path] = []
        self.lease: list[str] = []
        self.sequence = 0
        self.artifacts: list[dict] = []
        host_shelf = Path(os.environ.get("HOME", "")) / ".blaze" / "v2" / "toolchain"
        if host_shelf.is_dir():
            cached_shelf = self.root / ".toolchain-shelf"
            shutil.copytree(host_shelf, cached_shelf)
            self.shelf_source: Path | None = cached_shelf
        else:
            self.shelf_source = None

    def provenance(self) -> None:
        repository = HERE.parent
        commit, tree, diff = source_state(repository, self.given.command_timeout)
        (self.root / "source.patch").write_bytes(diff)
        identities = {}
        for name in ("blaze", "dist_local", "serve"):
            path = Path(getattr(self.given, name)).resolve(strict=True)
            identities[name] = {"path": str(path), "sha256": file_hash(path)}
        if self.given.module_builder is not None:
            path = Path(self.given.module_builder).resolve(strict=True)
            identities["module_builder"] = {"path": str(path), "sha256": self.builder_hash}
        project = Path(self.given.project).resolve()
        files = sorted(project.rglob("*"))
        fixture = hashlib.sha256()
        for path in files:
            if path.is_file() and not {"target", ".git"}.intersection(path.relative_to(project).parts):
                fixture.update(str(path.relative_to(project)).encode() + b"\0" + path.read_bytes())
        record = {"input": dataclasses.asdict(self.given), "commit": commit,
                  "tree": tree, "patch_sha256": hashlib.sha256(diff).hexdigest(),
                  "binaries": identities, "fixture_sha256": fixture.hexdigest(),
                  "resources": {"BLAZE_CORES": self.env("solo", False).get("BLAZE_CORES")}}
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
                # A child can exit after the snapshot that missed its final event.
                answer = predicate()
                if answer:
                    return answer
                raise Failed(f"{what}: children {ended} exited before the required evidence; see {self.root / 'logs'}")
            time.sleep(0.02)
        raise Failed(f"{what}: timed out; see {self.root / 'logs'}")

    def wait(self, process: subprocess.Popen) -> int:
        _, deadline, log = next(it for it in self.clients if it[0] is process)
        try:
            return process.wait(timeout=max(0, deadline - time.monotonic()))
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

    def _prepare_home(self, home: str) -> Path:
        home_dir = self.root / home
        shelf = getattr(self, "shelf_source", None)
        if shelf is not None and shelf.is_dir():
            dst = home_dir / ".blaze" / "v2" / "toolchain"
            if not dst.exists():
                dst.parent.mkdir(parents=True, exist_ok=True)
                try:
                    shutil.copytree(shelf, dst, copy_function=os.link)
                except OSError:
                    shutil.rmtree(dst, ignore_errors=True)
                    shutil.copytree(shelf, dst)
        return home_dir

    def env(self, home: str, dist: bool, **extra) -> dict:
        env = {k: v for k, v in os.environ.items() if not k.startswith(("CARGO", "RUST", "BLAZE_"))}
        env.update(self.given.env)
        if self.given.cores is not None:
            env["BLAZE_CORES"] = str(self.given.cores)
        elif "BLAZE_CORES" not in env and "BLAZE_CORES" in os.environ:
            env["BLAZE_CORES"] = os.environ["BLAZE_CORES"]
        home_dir = self._prepare_home(home)
        env.update(HOME=str(home_dir), TMPDIR=str(self.root / "tmp"), RUST_BACKTRACE="0", BLAZE_DIST_EVENTS=str(self.root / "logs" / "events.jsonl"))
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

    def plan(self, home: str) -> list[dict] | None:
        return evidence.unit_plan(Path(f"{self.report_path(home)}.plans"))

    @contextmanager
    def hold_manifest_lookup(self, home: str, unit: str):
        """Pause a real cache request after planning and before the unit can submit."""
        Path(f"{self.report_path(home)}.plans").unlink(missing_ok=True)
        requested, release = Event(), Event()

        def before_get(path):
            if path.startswith("/v1/list/manifests/") and not requested.is_set():
                plan = self.plan(home) or []
                if any(row["unit"] == unit and path == f"/v1/list/manifests/{row['unit_digest']}"
                       for row in plan):
                    requested.set()
                    return release.wait(self.given.command_timeout)
            return True

        with self.cache_proxy(f"{home}-{unit}-manifest", before_get=before_get) as carrier:
            try:
                yield carrier, requested, release
            finally:
                release.set()

    @contextmanager
    def cache_proxy(self, name, *, before_get=None, before_reply=None, before_body=None):
        """Forward the real cache protocol, with test barriers at its wire boundaries."""
        run = self

        class Carrier(BaseHTTPRequestHandler):
            protocol_version = "HTTP/1.1"

            def setup(self):
                self.request.settimeout(run.given.command_timeout)
                super().setup()

            def do_GET(self):
                if before_get is not None and not before_get(self.path):
                    self.send_error(504, f"{name}: release did not arrive for {self.path}")
                    return
                self.forward()

            def do_PUT(self):
                self.forward()

            def forward(self):
                size = int(self.headers["Content-Length"])
                with closing(HTTPConnection("127.0.0.1", run.port, timeout=run.given.command_timeout)) as upstream:
                    upstream.putrequest(self.command, self.path)
                    upstream.putheader("Content-Length", str(size))
                    upstream.putheader("Connection", "close")
                    if "If-Match" in self.headers:
                        upstream.putheader("If-Match", self.headers["If-Match"])
                    upstream.endheaders()
                    while size > 0:
                        chunk = self.rfile.read(min(size, 64 << 10))
                        if not chunk:
                            self.send_error(400, "incomplete cache request body")
                            return
                        upstream.send(chunk)
                        size -= len(chunk)
                    with upstream.getresponse() as response:
                        length = int(response.headers["Content-Length"])
                        reply = (self.path, response.status, length)
                        if self.command == "GET" and before_reply is not None and not before_reply(*reply):
                            self.send_error(504, f"{name}: release did not arrive for {self.path}")
                            return
                        self.send_response_only(response.status)
                        self.send_header("Content-Length", str(length))
                        self.send_header("Connection", "close")
                        self.end_headers()
                        if self.command == "GET" and before_body is not None:
                            self.wfile.flush()
                            if not before_body(*reply):
                                self.close_connection = True
                                return
                        shutil.copyfileobj(response, self.wfile)

        with ThreadingHTTPServer(("127.0.0.1", 0), Carrier) as server:
            driver = Thread(target=server.serve_forever, kwargs={"poll_interval": 0.05},
                            name=f"distv2-{name}", daemon=True)
            driver.start()
            try:
                yield f"127.0.0.1:{server.server_port}"
            finally:
                server.shutdown()
                driver.join(timeout=self.given.command_timeout)
                check(not driver.is_alive(), f"{name}: cache proxy stopped")

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
        triple = self.host
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

    def prepare_modules(self, home: str, project: Path) -> None:
        """Explicit D52 fixture setup, keyed per package revision, outside the compiler."""
        builder = self.given.module_builder
        if builder is None:
            return
        lock = project / "Cargo.lock"
        if not lock.is_file():
            raise Failed(f"module preparation requires the fixture lock: {lock}")
        staging = self.root / "bootstrap"
        store = staging / "records"
        for package, script in (("pm", False), ("shapes", True)):
            source = project / package
            digest = hashlib.sha256(bytes.fromhex(self.builder_hash) + bytes.fromhex(file_hash(lock)))
            digest.update(bytes.fromhex(file_hash(project / "Cargo.toml")))
            digest.update(package.encode())
            for path in sorted(source.rglob("*")):
                if path.is_symlink():
                    raise Failed(f"module preparation refuses a fixture symlink: {path}")
                if path.is_file():
                    relative = path.relative_to(source).as_posix().encode()
                    content = path.read_bytes()
                    digest.update(len(relative).to_bytes(8, "little") + relative)
                    digest.update(len(content).to_bytes(8, "little") + content)
            key = digest.hexdigest()
            if key in self.prepared_modules:
                continue
            work = staging / "work" / key
            request = {"checkout": str(project), "store": str(store), "work": str(work),
                       "tree": str(HERE.parent), "crates": [{"name": package, "version": "0.1.0", "features": []}],
                       "unsandboxed_build_scripts": False, "build_seconds": self.given.timeout}
            payload = {"build_scripts": request} if script else request
            log_path = self.root / "logs" / f"bootstrap-{package}-{key}.log"
            print(f"bootstrap D52 {package}: {key}; sandbox required; log {log_path}", flush=True)
            with log_path.open("w") as log:
                process = subprocess.Popen([builder, json.dumps(payload)], cwd=HERE.parent,
                                           env={**os.environ, **self.given.env}, stdout=log,
                                           stderr=subprocess.STDOUT, start_new_session=True)
                self.services.append(process)
                try:
                    code = process.wait(timeout=self.given.timeout)
                except subprocess.TimeoutExpired as why:
                    self.terminate(process, timeout=self.given.command_timeout)
                    raise Failed(f"module preparation {package} exceeded {self.given.timeout}s; log {log_path}") from why
                self.services.remove(process)
            require_build(code, log_path.read_text(), f"module preparation {package}", log_path)
            self.prepared_modules.add(key)
        destination = self._prepare_home(home) / ".blaze" / "v2"
        for kind in ("proc-macro", "build-script"):
            records = store / kind / "modules"
            if not records.is_dir():
                raise Failed(f"module preparation produced no records: {records}")
            for path in sorted(records.rglob("*")):
                if path.is_symlink():
                    raise Failed(f"module preparation produced a symlink: {path}")
                target = destination / kind / "modules" / path.relative_to(records)
                if path.is_dir():
                    target.mkdir(parents=True, exist_ok=True)
                elif target.exists():
                    if path.read_bytes() != target.read_bytes():
                        raise Failed(f"immutable bootstrap module differs at {target}")
                else:
                    target.parent.mkdir(parents=True, exist_ok=True)
                    shutil.copyfile(path, target)

    def build(self, home: str, dist: bool = True, wait: bool = True, project: Path | None = None,
              *, result_gate: Path | None = None, result_unit: str | None = None,
              keep_gate_lease: bool = False, cpus: list[int] | None = None, **extra):
        self.prepare_modules(home, project or self.project)
        if result_gate is not None:
            check(dist, "a result gate belongs to a distributed export")
            if result_gate not in self.result_gates:
                result_gate.mkdir(parents=True, exist_ok=False)
                self.result_gates.append(result_gate)
                if keep_gate_lease:
                    (result_gate / "keepalive").touch()
            extra["BLAZE_DIST_TEST_RESULT_GATE"] = str(result_gate.resolve())
            if result_unit is not None:
                extra["BLAZE_DIST_TEST_RESULT_UNIT"] = result_unit
        process, log_path = self.start_export(home, dist, project or self.project, cpus=cpus, **extra)
        if not wait:
            return process, log_path
        code = self.wait(process)
        return code, Path(log_path).read_text()

    def start_export(self, home: str, dist: bool, project: Path, *, cpus: list[int] | None = None,
                     **extra) -> tuple[subprocess.Popen, str]:
        """Launch after fixture modules have been prepared."""
        self.sequence += 1
        out = self.program(home, project)
        out.parent.mkdir(parents=True, exist_ok=True)
        env = {**self.env(home, dist, **extra), "CARGO_TARGET_DIR": str(self.target(home, project))}
        with (self.root / "logs" / f"{home}.{self.sequence}.log").open("x") as log:
            # Blaze pins its worker threads to the fastest available CPUs. A disjoint inherited
            # mask keeps the two processes from each selecting the same first CPU.
            previous = os.sched_getaffinity(0) if cpus is not None else None
            if cpus is not None:
                os.sched_setaffinity(0, cpus)
            try:
                process = subprocess.Popen(
                    [self.given.blaze, "export", "-p", self.given.program, "-o", str(out), "--stats", *self.given.args],
                    cwd=project,
                    env=env,
                    stdout=log,
                    stderr=subprocess.STDOUT,
                    start_new_session=True,
                )
                self.clients.append((process, time.monotonic() + self.given.timeout, log.name))
            finally:
                if previous is not None:
                    os.sched_setaffinity(0, previous)
        return process, log.name

    def target(self, home: str, project: Path | None = None) -> Path:
        """The per-worker output directory for this checkout."""
        return self.root / "targets" / home / (project or self.project).name

    def program(self, home: str, project: Path | None = None) -> Path:
        target = self.target(home, project).resolve()
        executable = (target / self.given.profile / self.given.program).resolve()
        if not executable.is_relative_to(target):
            raise Failed(f"artifact {executable} escapes build target {target}")
        return executable

    def behaviour(self, home: str, project: Path | None = None) -> tuple[str, str]:
        program = self.program(home, project)
        raw_hash = file_hash(program)
        ran = subprocess.run([str(program), *self.given.run], capture_output=True, text=True,
                             timeout=self.given.command_timeout)
        self.artifacts.append({"home": home, "project": str(project or self.project), "path": str(program),
                               "sha256": raw_hash, "stdout": ran.stdout, "stderr": ran.stderr, "exit": ran.returncode})
        (self.root / "artifacts.json").write_text(json.dumps(self.artifacts, indent=2) + "\n")
        check(ran.returncode == 0, f"{home} artifact runs successfully (exit {ran.returncode}; see artifacts.json)")
        return ran.stdout, raw_hash

    def stop(self) -> None:
        failures = []
        # Release test gates before resuming stopped processes; retain all capture evidence.
        for gate in self.result_gates:
            try:
                (gate / "release").touch()
            except OSError as why:
                failures.append(f"result gate {gate}: {why}")
        # A failure while a worker is SIGSTOPped must not strand it.
        for pid in self.suspended:
            try:
                os.kill(pid, signal.SIGCONT)
            except ProcessLookupError:
                pass
        for process in [*(p for p, _, _ in self.clients), *self.services]:
            try:
                self.terminate(process)
            except (OSError, subprocess.TimeoutExpired) as why:
                failures.append(f"pid {process.pid}: {why}")
        self.socket.unlink(missing_ok=True)
        if failures:
            raise Failed("owned-process cleanup failed: " + "; ".join(failures))

    def held_result(self, home: str, process: subprocess.Popen, gate: Path, after: int, *, suspend: bool) -> dict:
        pid = process.pid
        grant = self.until(f"{home} captures an actual uncommitted Result carrier",
                           lambda: evidence.held_result(gate, self.events()[after:], pid), [process])
        if suspend:
            os.kill(pid, signal.SIGSTOP)
            self.suspended.add(pid)
            self.until(f"{home} acknowledges SIGSTOP with its real result held",
                       lambda: stopped_status(pid), [process], timeout=self.given.command_timeout)
        check(not evidence.accepted(self.events()[after:], grant), "the exact captured result remains uncommitted at its gate")
        return grant

    def peer_result_while_held(self, gate: Path, a: subprocess.Popen, b: subprocess.Popen, after: int) -> dict:
        """Keep a real lead batch open until a second holder commits disjoint work."""
        def captured():
            events = self.events()[after:]
            for row in events:
                if row["event"] == "result_held" and row["pid"] in (a.pid, b.pid):
                    return evidence.held_result(gate, events, row["pid"])
            return None

        grant = self.until("one worker captures a real mathy Result", captured, [a, b])
        peer = b if grant["pid"] == a.pid else a
        self.until("the peer submits its own mathy result before the first worker releases",
                   lambda: evidence.peer_result_before_release(self.events()[after:], grant, peer.pid), [a, b])
        return grant

    def release_after_peer_result(self, gate: Path, a: subprocess.Popen, b: subprocess.Popen, after: int) -> None:
        grant = self.peer_result_while_held(gate, a, b, after)
        (gate / "release").touch()
        self.until("the released mathy result is accepted by Client and the coordinator",
                   lambda: evidence.gate_answer(gate, grant, refused=False)
                   and evidence.accepted(self.events()[after:], grant), [a, b])

    def fault_pair(self, gate: Path, after: int):
        a, a_log = self.build("a", wait=False, result_gate=gate, result_unit="mathy", keep_gate_lease=True)
        b, b_log = self.build("b", wait=False, result_gate=gate, result_unit="mathy", keep_gate_lease=True)
        grant = self.peer_result_while_held(gate, a, b, after)
        a_build, b_build = ("a", a, a_log), ("b", b, b_log)
        victim, peer = (a_build, b_build) if grant["pid"] == a.pid else (b_build, a_build)
        return grant, victim, peer


def file_hash(path: Path) -> str:
    with path.open("rb") as source:
        return hashlib.file_digest(source, "sha256").hexdigest()


def verify_run_identity(run: Run) -> None:
    for name, identity in run.identities.items():
        check(file_hash(Path(identity["path"])) == identity["sha256"], f"{name} binary did not change during acceptance")
    provenance = json.loads((run.root / "provenance.json").read_text())
    commit, tree, diff = source_state(HERE.parent, run.given.command_timeout)
    check(commit == provenance["commit"] and tree == provenance["tree"]
          and hashlib.sha256(diff).hexdigest() == provenance["patch_sha256"],
          "source commit, tree and patch did not change during acceptance")


def stopped_status(pid: int) -> bool:
    try:
        child, status = os.waitpid(pid, os.WUNTRACED | os.WNOHANG)
    except ChildProcessError as why:
        raise Failed(f"daemon {pid} exited before acknowledging SIGSTOP") from why
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


def private_edit_handoff(run: Run, baseline: tuple[str, str]) -> None:
    source = run.project / "app" / "src" / "main.rs"
    original = source.read_text()
    marker = "wrapping_mul(0x100000001b3)"
    check(original.count(marker) == 1, "the private main body contains one edit marker")
    edit(source, marker, "wrapping_mul(0x100000001b5)")
    try:
        # Both homes are warm. Let the helper publish the edited final unit before the
        # primary sees this revision, so primary cannot win a lead race.
        rows_before = {home: len(run.report(home)) for home in ("a", "b")}
        mark = len(run.events())
        code, output = run.build("a")
        require_build(code, output, "private edit helper builds", run.root / "logs" / f"a.{run.sequence}.log")
        helper = run.report("a")[rows_before["a"]:]
        helper_events = run.events()[mark:]
        wanted = control(run)
        check(wanted[0] != baseline[0] and wanted[1] != baseline[1], "the private body edit changes stdout and artifact bytes")
        check(run.behaviour("a") == wanted, "the private edit helper equals solo in bytes and behavior")

        start_profile = run.project / ".blaze" / "v2" / "start.txt"
        start_profile.unlink(missing_ok=True)
        code, output = run.build("b")
        require_build(code, output, "private edit primary builds", run.root / "logs" / f"b.{run.sequence}.log")
        saved_profile = run.root / "logs" / "private-edit-primary.start.txt"
        shutil.copyfile(start_profile, saved_profile)
        primary = run.report("b")[rows_before["b"]:]
        evidence.cached(primary)
        evidence.verified_reach_handoff(run.root / "blobs", run.given.program, helper, primary, helper_events, saved_profile)
        evidence.no_local_emission("private edit primary", output)
        check(run.behaviour("b") == wanted, "the private edit primary consumes helper reachability and equals solo in bytes and behavior")
    finally:
        source.write_text(original)


def latency_export(run: Run, placements: dict[str, list[int]], projects: dict[str, Path], *,
                   dist: bool) -> list[dict]:
    """Time invocation to successful exit. Qualify that time with the artifact oracle after both exit."""
    started = time.monotonic_ns()
    pending = {}
    for home, cpus in placements.items():
        process, log = run.start_export(home, dist, projects[home], cpus=cpus, BLAZE_CORES=str(len(cpus)))
        pending[home] = process, Path(log)
    deadline = started + run.given.timeout * 1_000_000_000
    completions = []
    while pending:
        for home, (process, log) in tuple(pending.items()):
            code = process.poll()
            if code is None:
                continue
            elapsed = time.monotonic_ns() - started
            if code != 0:
                require_build(code, log.read_text(), f"latency export {home}", log)
            completions.append({"home": home, "cpus": placements[home], "elapsed_ns": elapsed, "log": str(log)})
            del pending[home]
        if not pending:
            break
        if time.monotonic_ns() >= deadline:
            raise Failed(f"latency export timed out with {list(pending)} still running; see {run.root / 'logs'}")
        time.sleep(0.001)
    return completions


def latency_round(run: Run, placements: dict[str, dict[str, list[int]]], modes: list[str],
                  repetition: int) -> list[dict]:
    projects = {home: run.root / "checkouts" / home for by_home in placements.values() for home in by_home}
    for project in projects.values():
        shutil.copytree(run.given.project, project, ignore=shutil.ignore_patterns("target", ".git", ".blaze"))
    edited = {home: project / "mathy" / "src" / "lib.rs" for home, project in projects.items()}
    run.provenance()
    run.host = host_triple(run.given.blaze, run.env("solo", False), run.given.command_timeout, run.root / "logs")
    run.start_services()
    for mode in modes:
        dist = mode == "two_workers"
        for home, cpus in placements[mode].items():
            # Seed each worker's own persistent state. Seed times are outside the measured intervals.
            code, output = run.build(home, dist=dist, project=projects[home], cpus=cpus, BLAZE_CORES=str(len(cpus)))
            require_build(code, output, f"seed {home}", run.root / "logs" / f"{home}.{run.sequence}.log")
    solo = run.behaviour("solo", projects["solo"])
    n = int(run.given.run[0])
    expected = (f"caught {str(n % 2 == 1).lower()}, dropped {n}, script says from the script\n"
                "included as it was checked in\nseen by the macro 1\n")
    check(solo[0].startswith("40 functions, ") and solo[0].endswith(expected),
          "seed executes 40 functions and the expected unwind, drop, script, include and macro")
    for home in placements["two_workers"]:
        check(run.behaviour(home, projects[home]) == solo, f"seed {home} equals solo in bytes and behavior")

    samples = []
    for case in ("no_op", "single_function_edit"):
        if case == "single_function_edit":
            for source in edited.values():
                check(source.read_text().count('format!("3:{sum}') == 1,
                      "one function contains the body-edit marker")
                edit(source, 'format!("3:{sum}', 'format!("3 edited:{sum}')
        source_hash = file_hash(edited["solo"])
        check(all(file_hash(source) == source_hash for source in edited.values()),
              f"{case} uses the same body text in every checkout")
        rows_before = {home: len(run.report(home)) for home in placements["two_workers"]}
        results = {}
        for mode in modes:
            # Removing the previous reports outside the timer proves that each copy comes from this export.
            state = {home: projects[home] / ".blaze" / "v2" for home in placements[mode]}
            for directory in state.values():
                for name in ("profile.txt", "start.txt"):
                    (directory / name).unlink(missing_ok=True)
            load_before = os.getloadavg()
            completions = latency_export(run, placements[mode], projects, dist=mode == "two_workers")
            load_after = os.getloadavg()
            for completion in completions:
                home = completion["home"]
                profiles = {}
                for name in ("profile.txt", "start.txt"):
                    path = run.root / "logs" / f"{case}.{home}.{name}"
                    shutil.copyfile(state[home] / name, path)
                    profiles[name] = str(path)
                stdout, digest = run.behaviour(home, projects[home])
                completion.update(stdout=stdout, sha256=digest, profiles=profiles)
            results[mode] = {
                "repetition": repetition, "case": case, "mode": mode,
                "source_sha256": source_hash, "cores": run.given.cores,
                "first_artifact_ns": min(item["elapsed_ns"] for item in completions),
                "workers": completions, "load_before": load_before, "load_after": load_after,
            }
        reference = results["solo"]["workers"][0]
        for result in results.values():
            for artifact in result["workers"]:
                check((artifact["stdout"], artifact["sha256"]) == (reference["stdout"], reference["sha256"]),
                      f"{case} {artifact['home']} equals solo in bytes and behavior")
        rows = [row for home, before in rows_before.items() for row in run.report(home)[before:]]
        if case == "no_op":
            check((reference["stdout"], reference["sha256"]) == solo, "no-op matches the seeded artifact")
            check(not rows, "distributed no-op runs no unit")
            for worker in results["two_workers"]["workers"]:
                evidence.no_local_emission(f"no-op {worker['home']}", Path(worker["log"]).read_text())
        else:
            check(reference["stdout"] != solo[0] and reference["stdout"].endswith(expected),
                  "the body edit changes the function-result digest and preserves the expected runtime behavior")
            check(any(row["unit"] == "mathy" and row["source"] == "executed" for row in rows),
                  "the changed unit executes through the coordinator")
            evidence.no_duplicate_compile(rows)
            evidence.publication(run.root / "blobs", rows, run.events())
        for worker in results["two_workers"]["workers"]:
            home = worker["home"]
            worker["work"] = distributed_work(run.report(home)[rows_before[home]:], Path(worker["profiles"]["start.txt"]))
        samples.extend(results.values())
    verify_run_identity(run)
    return samples


def dev_latency(given: Input) -> None:
    if not sys.platform.startswith("linux"):
        raise Failed("dev latency requires Linux CPU affinity")
    previous = os.sched_getaffinity(0)
    cores = given.cores
    if cores is None or len(previous) < cores:
        raise Failed(f"dev latency needs {cores} CPUs in the runner affinity; available {sorted(previous)}")
    cpus = sorted(previous)[:cores]
    root = Path(given.scratch).resolve()
    root.mkdir(parents=True, exist_ok=True)
    if any(root.iterdir()):
        raise Failed(f"scratch {root} is not empty; use a new run directory")
    samples = []
    os.sched_setaffinity(0, cpus)
    try:
        for repetition in range(given.repetitions):
            order = cpus if repetition % 2 == 0 else list(reversed(cpus))
            split = cores // 2
            placements = {"solo": {"solo": cpus}, "two_workers": {"a": order[:split], "b": order[split:]}}
            modes = list(placements) if repetition % 2 == 0 else list(reversed(placements))
            scratch = root / f"run-{repetition + 1}"
            env = {**given.env, "XDG_CACHE_HOME": str(scratch / "machine-cache")}
            run = Run(dataclasses.replace(given, scratch=str(scratch), env=env))
            try:
                samples.extend(latency_round(run, placements, modes, repetition + 1))
            finally:
                run.stop()
    finally:
        os.sched_setaffinity(0, previous)
    report = {"cpus": cpus, "poll_interval_ns": 1_000_000,
              "endpoint": "first successful export exit whose artifact subsequently passes the oracle",
              "samples": samples}
    (root / "dev-latency.json").write_text(json.dumps(report, indent=2) + "\n")
    print("distv2: dev latency checks passed", flush=True)


def overlap_fixture(path: Path) -> None:
    """Two reachable sibling leaves, with no build scripts or external packages."""
    path.mkdir()
    (path / "Cargo.toml").write_text('[workspace]\nmembers = ["app", "overlap_a", "overlap_b"]\nresolver = "2"\n')
    for name, body in (("overlap_a", "n.wrapping_mul(7).wrapping_add(3)"),
                       ("overlap_b", "n.rotate_left(3) ^ 0x5a")):
        (path / name / "src").mkdir(parents=True)
        (path / name / "Cargo.toml").write_text(f'[package]\nname = "{name}"\nversion = "0.1.0"\nedition = "2024"\n')
        (path / name / "src/lib.rs").write_text(f"#[inline(never)]\npub fn work(n: u64) -> u64 {{ {body} }}\n")
    (path / "app/src").mkdir(parents=True)
    (path / "app/Cargo.toml").write_text(
        '[package]\nname = "app"\nversion = "0.1.0"\nedition = "2024"\n'
        '[dependencies]\noverlap_a = { path = "../overlap_a" }\noverlap_b = { path = "../overlap_b" }\n')
    (path / "app/src/main.rs").write_text(
        'fn main() {\n'
        '    let n = std::env::args().nth(1).unwrap().parse::<u64>().unwrap();\n'
        '    println!("cache overlap: {} {}", overlap_a::work(n), overlap_b::work(n));\n'
        '}\n')
    # All requests see the same input workspace before planning starts.
    (path / "Cargo.lock").write_text('version = 4\n\n[[package]]\nname = "app"\nversion = "0.1.0"\n'
        'dependencies = ["overlap_a", "overlap_b"]\n\n[[package]]\nname = "overlap_a"\nversion = "0.1.0"\n'
        '\n[[package]]\nname = "overlap_b"\nversion = "0.1.0"\n')


class HeldPack:
    """Withhold a real pack body and let the earlier manifest miss through only then."""

    def __init__(self, missing, packs, timeout, *, speculative_miss, event_mark):
        self.missing = f"/v1/list/manifests/{missing}"
        self.packs = {f"/v1/blob/packs/{name}.pack": path.stat().st_size for name, path in packs.items()}
        self.timeout = timeout
        self.speculative_miss = speculative_miss
        self.event_mark = event_mark
        self.held = Event()
        self.release = Event()
        self.lock = Lock()
        self.receipt = None
        self.misses = []
        self.miss_before_pack = False
        self.expired = False
        self.error = None
        self.requests = []

    def before_reply(self, path, status, size):
        if path != self.missing:
            return True
        with self.lock:
            miss = {"path": path, "status": status, "bytes": size, "at_ns": time.monotonic_ns()}
            self.misses.append(miss)
            if status != 404:
                self.error = f"the earlier manifest was not absent: HTTP {status}"
                return False
            if self.speculative_miss and len(self.misses) == 1:
                miss["speculative"] = True
                return True
            if len(self.misses) != (2 if self.speculative_miss else 1):
                self.error = "the earlier leaf looked up its absent manifest more than expected"
                return False
        active = self.held.wait(self.timeout)
        with self.lock:
            miss["forwarded_ns"] = time.monotonic_ns()
            self.miss_before_pack = not active
        # The serial control still gets the origin's real 404, so the harness can
        # reject its actual early grant instead of attributing a fabricated HTTP error.
        return True

    def before_body(self, path, status, size):
        if path not in self.packs:
            return True
        with self.lock:
            self.requests.append(path)
            if self.release.is_set():
                return True
            if self.receipt is not None or status != 200 or size != self.packs[path] or size <= 8:
                self.error = f"unexpected additional or invalid later pack response {path}: {status}, {size} bytes"
                return False
            self.receipt = {"path": path, "status": status, "bytes": size, "at_ns": time.monotonic_ns(),
                            "event_mark": self.event_mark()}
            self.held.set()
        if self.release.wait(self.timeout):
            return True
        with self.lock:
            self.expired = True
        return False

    def observed(self):
        with self.lock:
            check(self.error is None, self.error or "the proxy held exactly one real later pack response")
            check(self.receipt is not None and not self.expired and not self.release.is_set(),
                  "the later pack body is still held by the proxy")
            check(len(self.misses) == (2 if self.speculative_miss else 1) and not self.miss_before_pack
                  and self.misses[-1]["forwarded_ns"] >= self.receipt["at_ns"],
                  "the earlier on-demand lookup saw its real 404 after the later body was held")
            return self.receipt, self.misses


def cache_overlap(given: Input) -> None:
    """A causal check on the actual cache carrier and compiler, with no elapsed-time oracle."""
    project = Path(given.scratch).resolve() / "project"
    given = dataclasses.replace(given, project=str(project), program="app", run=("11",), module_builder=None)
    run = Run(given)
    run.project = project
    overlap_fixture(project)
    try:
        run.provenance()
        resource_env = run.env("solo", False)
        check(bool(resource_env.get("BLAZE_CORES", "").strip()), "overlap requires a per-process core budget")
        run.host = host_triple(given.blaze, resource_env, given.command_timeout, run.root / "logs")
        run.start_services()
        wanted = control(run)
        check(wanted[0] == "cache overlap: 80 2\n", "the solo artifact runs both sibling leaves")
        seed, log = run.build("seed", wait=False)
        require_build(run.wait(seed), Path(log).read_text(), "seed the real unit publications", Path(log))
        check(run.behaviour("seed") == wanted, "seed artifact equals solo in bytes and behavior")
        plan = run.plan("seed")
        check(plan is not None, "seed reported its complete unit plan")
        ordered = [row["unit"] for row in plan]
        leaves = [i for i, name in enumerate(ordered) if name in ("overlap_a", "overlap_b")]
        check(len(leaves) == 2 and leaves[1] == leaves[0] + 1 and ordered[-1] == "app",
              f"sibling leaves are consecutive and the product is last: {ordered}")
        earlier, later = [plan[i] for i in leaves]
        first, second = earlier["unit_digest"], later["unit_digest"]
        check(bool(earlier["plan_names"] and later["plan_names"]), "both leaves own real planned work")
        blobs = run.root / "blobs"
        seed_rows = run.report("seed")
        check([row["unit_digest"] for row in seed_rows] == [row["unit_digest"] for row in plan],
              "the seed reported every unit in admission order")
        evidence.publication(blobs, seed_rows, run.events())
        seeded = {row["unit_digest"]: evidence.manifest(blobs, row["unit_digest"])[0] for row in seed_rows}
        check(bool(seeded[first] and seeded[second]) and not set(seeded[first]).intersection(seeded[second]),
              "the earlier miss and later hit have distinct nonempty pack sets")
        earlier_manifest = blobs / "manifests" / first
        later_manifest = blobs / "manifests" / second
        unchanged_later = later_manifest.read_bytes()
        earlier_manifest.rename(run.root / "seed-earlier.manifest")
        after = len(run.events())
        held = HeldPack(first, seeded[second], given.command_timeout, speculative_miss=leaves[0] != 0,
                        event_mark=lambda: len(run.events()))
        result_gate = run.root / "gates" / "overlap-earlier-result"
        with run.cache_proxy("sibling-pack-overlap", before_reply=held.before_reply, before_body=held.before_body) as carrier:
            try:
                replay, replay_log = run.build("replay", wait=False, result_gate=result_gate, result_unit=earlier["unit"],
                                               keep_gate_lease=True, BLAZE_DIST_BLOBS=carrier)

                def held_before_grant():
                    events = run.events()
                    packet = held.receipt
                    before_hold = events[after:packet["event_mark"]] if packet is not None else events[after:]
                    if any(event["event"] == "grant" and event["unit_digest"] == first
                           and event["pid"] == replay.pid for event in before_hold):
                        raise Failed("the earlier grant started before the later pack's headers and body gate")
                    if held.error is not None:
                        raise Failed(held.error)
                    return held.held.is_set()

                run.until("the later leaf's actual pack response reaches its body gate", held_before_grant, [replay])
                end = held.receipt["at_ns"] / 1e9 + given.command_timeout

                def while_held(what, predicate):
                    left = end - time.monotonic()
                    check(left > 0 and not held.expired and not held.release.is_set(), f"{what}: the pack gate still has time")
                    return run.until(what, predicate, [replay], timeout=left)

                grant = while_held("the earlier miss gets a real grant and captures its Result",
                    lambda: evidence.held_result(result_gate, run.events()[after:], replay.pid))
                check(grant["unit_digest"] == first, "the captured grant belongs to the missing earlier leaf")
                (result_gate / "release").touch()
                while_held("the earlier Result is committed by the coordinator and acknowledged by its Client",
                    lambda: evidence.overlap_accepted(run.events()[after:], grant) is not None
                    and evidence.gate_answer(result_gate, grant, refused=False))
                row = while_held("the earlier leaf reports a successful complete publication",
                    lambda: next((row for row in evidence.jsonl(run.report_path("replay"), live=True)
                                  if row["unit_digest"] == first), None))
                events = run.events()[after:]
                evidence.overlap_publication(blobs, result_gate, row, grant, events)
                packet, miss = held.observed()
                (run.root / "overlap-before-release.json").write_text(json.dumps({
                    "held": packet, "missing": miss, "later_unit": second, "earlier_grant": grant,
                    "earlier_report": row, "event_start": after, "events_read": len(events),
                    "before_release_ns": time.monotonic_ns(),
                }, indent=2) + "\n")
            finally:
                held.release.set()
                (run.root / "overlap-transport.json").write_text(json.dumps({
                    "held": held.receipt, "missing": held.misses, "pack_requests": held.requests,
                    "miss_before_pack": held.miss_before_pack, "expired": held.expired, "error": held.error,
                    "released_ns": time.monotonic_ns(),
                }, indent=2) + "\n")
            replay_code = run.wait(replay)
            replay_output = Path(replay_log).read_text()
            require_build(replay_code, replay_output, "replay finishes after releasing the cached body", Path(replay_log))
        check(run.behaviour("replay") == wanted, "overlap replay artifact equals solo in bytes and behavior")
        replay_plan = run.plan("replay")
        check(replay_plan is not None, "replay reported its complete plan")
        evidence.same_unit_plans(plan, replay_plan)
        rows = run.report("replay")
        check([row["unit_digest"] for row in rows] == [row["unit_digest"] for row in seed_rows],
              "replay preserves the original admission order")
        cached = [row for row in rows if row["unit_digest"] != first]
        evidence.cached(cached)
        # Release's summary includes actual worker compilation. Dev's counts are only
        # the final assembly walk, whose complete input must have arrived from the units.
        release_compiled = row["compiled_for_work"] + row["tail_compiled"] if "--release" in given.args else 0
        evidence.no_local_emission("overlap replay", replay_output, already_compiled=release_compiled)
        check(not any(event["event"] == "grant" and event["unit_digest"] != first for event in run.events()[after:]),
              "no cached unit received a grant, including the later leaf")
        check(later_manifest.read_bytes() == unchanged_later, "the later leaf retains its exact original manifest")
        check(held.requests.count(packet["path"]) == 1, "the replay consumes the held pack without fetching it again")
        for row in rows:
            unit = row["unit_digest"]
            actual, _ = evidence.manifest(blobs, unit)
            check(evidence.logical_outputs(blobs, actual) == evidence.logical_outputs(blobs, seeded[unit]),
                  f"{row['unit']}: exact published record keys, kinds and values equal the seed")
        verify_run_identity(run)
        print("distv2: native cache overlap passed", flush=True)
    finally:
        run.stop()


def committed_pack_refused(run: Run, home: str, policy: str, target: Path) -> None:
    """Neither local policy may recompile an already-published, corrupted key."""
    code, text = run.build(home, BLAZE_DIST_LOCAL=policy)
    check(code != 0 and "distributed build failed" in text and target.stem in text,
          f"{policy} must refuse committed pack {target.stem} by name, not recompile it")
    check(not run.program(home).exists(), f"{policy} must emit no artifact from a corrupt committed pack")


def faults(run: Run, edited: Path) -> None:
    # Positive causal control: capture a real result and release it before its lease expires.
    # This uses the default long lease; the stale case below adds expiry and replacement.
    edit(edited, 'format!("9:{sum}', 'format!("9 gated valid:{sum}')
    wanted = control(run)
    valid_gate = run.root / "gates" / "valid-result"
    mark = len(run.events())
    valid, valid_log = run.build("a", wait=False, result_gate=valid_gate, result_unit="mathy")
    grant = run.held_result("a", valid, valid_gate, mark, suspend=False)
    (valid_gate / "release").touch()
    run.until("the released valid Result is accepted by the real coordinator and Client",
              lambda: evidence.accepted(run.events()[mark:], grant)
              and evidence.gate_answer(valid_gate, grant, refused=False), [valid])
    code = run.wait(valid)
    check(code == 0, f"the valid-result control finishes: {Path(valid_log).read_text()[-800:] if code else ''}")
    check(not evidence.stale_refused(run.events()[mark:], grant), "the valid captured result was not refused as stale")
    check(run.behaviour("a") == wanted, "the released valid-result artifact equals solo in bytes and behavior")
    edit(edited, 'format!("9 gated valid:{sum}', 'format!("9:{sum}')

    # Establish both holders in the same attempt before killing the lead with uncommitted work.
    edit(edited, 'format!("7:{sum}', 'format!("7 lost:{sum}')
    wanted = control(run)
    lost_gate = run.root / "gates" / "killed-result"
    mark = len(run.events())
    grant, (_, lost, lost_log), (survivor_home, survivor, survivor_log) = run.fault_pair(lost_gate, mark)
    killed_at = len(run.events()) - mark
    os.killpg(grant["pid"], signal.SIGKILL)
    check(run.wait(lost) == -signal.SIGKILL, f"worker {grant['pid']} exits from SIGKILL (log {lost_log})")
    run.until("survivor commits every name held by the killed worker",
              lambda: evidence.recovered(run.events()[mark:], grant, accepted_after=killed_at), [survivor])
    code = run.wait(survivor)
    check(code == 0, f"the survivor finishes within the bound: {Path(survivor_log).read_text()[-800:] if code else ''}")
    check(not evidence.accepted(run.events()[mark:], grant), "the killed worker's outstanding result was never committed")
    check(run.behaviour(survivor_home) == wanted, "worker-death recovery equals solo in bytes and behavior")
    edit(edited, 'format!("7 lost:{sum}', 'format!("7:{sum}')

    # A short lease is a test input, not a timing inference: keep the victim stopped until
    # the coordinator accepts replacement results for every one of its outstanding names.
    run.terminate(run.coordinator)
    run.lease = ["2000"]
    run.start_coordinator()
    edit(edited, 'format!("8:{sum}', 'format!("8 fence:{sum}')
    wanted = control(run)
    stale_gate = run.root / "gates" / "stale-result"
    mark = len(run.events())
    grant, (_, stalled, stalled_log), (_, healthy, healthy_log) = run.fault_pair(stale_gate, mark)
    stopped_at = len(run.events()) - mark
    os.kill(grant["pid"], signal.SIGSTOP)
    run.suspended.add(grant["pid"])
    run.until("the lead acknowledges SIGSTOP with its result still held",
              lambda: stopped_status(grant["pid"]), [stalled, healthy], timeout=run.given.command_timeout)
    run.until("replacement commits while stale holder remains stopped",
              lambda: evidence.recovered(run.events()[mark:], grant, accepted_after=stopped_at), [healthy])
    check(not evidence.accepted(run.events()[mark:], grant), "the stopped lease was uncommitted when its work was taken back")
    (stale_gate / "release").touch()
    resumed_at = len(run.events()) - mark
    os.kill(grant["pid"], signal.SIGCONT)
    run.suspended.remove(grant["pid"])
    run.until("resumed holder's actual stale result is explicitly refused by coordinator and Client",
              lambda: evidence.stale_refused(run.events()[mark:], grant, refused_after=resumed_at)
              and evidence.gate_answer(stale_gate, grant, refused=True), [stalled])
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
        for home, policy in (("corrupt-never", "never"), ("corrupt-fallback", "fallback")):
            committed_pack_refused(run, home, policy, target)
            check(file_hash(target) != target.stem,
                  f"{policy} left the same immutable-key corruption in place; no repair was attempted")
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
    if given.dev_latency:
        dev_latency(given)
        return
    if given.cache_overlap:
        cache_overlap(given)
        return
    run = Run(given)
    run.project = run.root / "project"
    shutil.copytree(given.project, run.project, ignore=shutil.ignore_patterns("target", ".git"), dirs_exist_ok=True)
    edited = run.project / "mathy" / "src" / "lib.rs"
    try:
        run.provenance()
        resource_env = run.env("solo", False)
        check(bool(resource_env.get("BLAZE_CORES", "").strip()),
              "root supplied an explicit per-process core budget before starting children")
        run.host = host_triple(given.blaze, resource_env, given.command_timeout, run.root / "logs")
        run.start_services()
        code, text = run.build("solo", dist=False)
        require_build(code, text, "solo builds", run.root / "logs" / f"solo.{run.sequence}.log")
        solo = run.behaviour("solo")
        print("     solo:", solo[0].strip().replace("\n", " | "), solo[1][:16], flush=True)
        n = int(given.run[0])
        expected = (f"caught {str(n % 2 == 1).lower()}, dropped {n}, script says from the script\n"
                    "included as it was checked in\nseen by the macro 1\n")
        check(solo[0].startswith("40 functions, ") and solo[0].endswith(expected),
              "solo executes 40 functions, the expected unwind/drop, script, included file and proc macro")

        overlap = run.root / "gates" / "shared-mathy"
        mark = len(run.events())
        a, a_log = run.build("a", wait=False, result_gate=overlap, result_unit="mathy")
        b, b_log = run.build("b", wait=False, result_gate=overlap, result_unit="mathy")

        def initial_plans():
            first, second = run.plan("a"), run.plan("b")
            return (first, second) if first is not None and second is not None else None

        evidence.same_unit_plans(*run.until("both workers report their initial plans", initial_plans, [a, b]))
        check(True, "both workers planned the same units")
        run.release_after_peer_result(overlap, a, b, mark)
        code_a, code_b = run.wait(a), run.wait(b)
        require_build(code_a, Path(a_log).read_text() if code_a else "", "worker a builds", Path(a_log))
        require_build(code_b, Path(b_log).read_text() if code_b else "", "worker b builds", Path(b_log))
        rows_a, rows_b = run.report("a"), run.report("b")
        check(bool(rows_a) and bool(rows_b), f"both report units: {len(rows_a)} and {len(rows_b)}")
        compiled_a, compiled_b = sums(rows_a, "compiled_for_work"), sums(rows_b, "compiled_for_work")
        print(f"     a compiled {compiled_a} for the work, b {compiled_b}", flush=True)
        check(compiled_a > 0 and compiled_b > 0, "both workers compiled functions for the others")
        for name, rows in (("a", rows_a), ("b", rows_b)):
            evidence.fixture_units(name, rows, release="--release" in given.args)
            check(True, f"worker {name} reports the fixture units for this backend")
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
        check(bool(shared), "both workers actually compile the same unit in the same execution and attempt")
        evidence.publication(run.root / "blobs", rows_a + rows_b, run.events())
        if given.release_fold:
            evidence.release_fold(run.root / "blobs", rows_a + rows_b, run.events())
            check(True, "two holders actually emitted LLVM work and two holders emitted folded bundles in shared executions")
        check(True, "no granted template was recompiled in the tail or by another worker; every raw emitted key was committed in the full manifest")
        for name, behaviour in (("a", run.behaviour("a")), ("b", run.behaviour("b"))):
            check(behaviour[0] == solo[0], f"worker {name}'s program behaves as solo's")
            check(behaviour[1] == solo[1], f"worker {name}'s program is solo's, byte for byte")

        code, text = run.build("cold")
        check(code == 0, f"a fresh process builds from the cache: {text[-400:] if code else ''}")
        cold_export = text
        rows = run.report("cold")
        published = [row for row in rows if row["source"] == "published"]
        print(f"     cold: {len(published)}/{len(rows)} published; compiled {sums(rows, 'tail_compiled')} of {sums(rows, 'instances')}", flush=True)
        check(len(published) == len(rows) and rows, f"every unit of the cold export was published: {len(published)}/{len(rows)}")
        evidence.cached(rows)
        evidence.publication(run.root / "blobs", rows, run.events())
        check(True, "cold units schedule no execution or grants and compile no work or tail keys")
        behaviour = run.behaviour("cold")
        check(behaviour[0] == solo[0] and behaviour[1] == solo[1], "its program is solo's, byte for byte")

        before = len(run.report("cold"))
        code, warm_export = run.build("cold")
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
        if "--release" not in given.args:
            private_edit_handoff(run, solo)

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

        # The older export has no live unit between finishing mathy and submitting app.
        # The coordinator must still know which source the next old unit belongs to.
        older_at, newer_at = run.root / "project-older", run.root / "project-newer"
        for at, change in ((older_at, 'format!("6 older:{sum}'), (newer_at, 'format!("6 newer:{sum}')):
            shutil.copytree(run.project, at, ignore=shutil.ignore_patterns("target", ".git"))
            edit(at / "mathy" / "src" / "lib.rs", 'format!("6:{sum}', change)
        mark = len(run.events())
        revision_gate = run.root / "gates" / "newer-revision"
        with run.hold_manifest_lookup("a", "app") as (carrier, requested, release):
            older, older_log = run.build("a", wait=False, project=older_at, BLAZE_DIST_PR="18", BLAZE_DIST_SOURCE="old",
                                         BLAZE_DIST_BLOBS=carrier)
            run.until("the old export reaches app's manifest lookup with no live unit", requested.is_set, [older])
            older_plan = run.plan("a")
            check(older_plan is not None, "the held lookup belongs to the old export's complete plan")
            old_mathy = next(row["unit_digest"] for row in older_plan if row["unit"] == "mathy")
            old_app = next(row["unit_digest"] for row in older_plan if row["unit"] == "app")
            check(any(row["event"] == "done" and row["unit_digest"] == old_mathy and row["outcome"] == "Passed"
                      for row in run.events()[mark:]), "old mathy is terminal before the newer source is admitted")
            newer, newer_log = run.build("b", wait=False, project=newer_at, result_gate=revision_gate, result_unit="mathy",
                                         BLAZE_DIST_PR="18", BLAZE_DIST_SOURCE="new")
            run.held_result("newer revision", newer, revision_gate, mark, suspend=False)
            release.set()
            code_older = run.wait(older)
            old_error = Path(older_log).read_text()
            check(code_older != 0 and "superseded:" in old_error,
                  f"the old revision's next unit is refused as superseded: {older_log}")
            check(not any(row["event"] == "grant" and row["unit_digest"] == old_app for row in run.events()[mark:]),
                  "the superseded app received no grant")
            (revision_gate / "release").touch()
        code_newer = run.wait(newer)
        require_build(code_newer, Path(newer_log).read_text() if code_newer else "", "the newer revision builds", Path(newer_log))
        check(run.behaviour("b", newer_at) == control(run, newer_at), "the newer revision equals its solo control")

        if given.faults:
            faults(run, edited)
        replay_failures = []
        for phase, output in (("cold", cold_export), ("warm", warm_export)):
            try:
                evidence.no_local_emission(phase, output)
            except Failed as error:
                replay_failures.append(str(error))
        check(not replay_failures, "\n".join(replay_failures) if replay_failures else "cold and warm replay emitted no local code")
        verify_run_identity(run)
        print("distv2: every check passed" if given.faults else "distv2: non-fault checks passed; fault acceptance NOT RUN", flush=True)
    finally:
        run.stop()


HOST_LEAVES = 512
HOST_MASK = (1 << 64) - 1
HOST_FNV = 0x100000001B3
HOST_SEED = 0xCBF29CE484222325
HOST_PROGRAM_INPUT = 17
HOST_MESSAGE_LIMIT = 8 << 20
HOST_PACKAGE = "host_proof"
HOST_EDIT_LEAF = HOST_LEAVES // 2


@dataclasses.dataclass(frozen=True)
class HostNode:
    checkout: str
    scratch: str
    blaze: str
    python: str
    runner: str
    socket: str
    blobs: str


@dataclasses.dataclass(frozen=True)
class HostHelper(HostNode):
    alias: str


@dataclasses.dataclass(frozen=True)
class HostWorker(HostNode):
    role: str
    namespace: str
    cores: int
    timeout: int
    benchmark: bool = dataclasses.field(default=False, kw_only=True)


@dataclasses.dataclass(frozen=True)
class HostController(HostWorker):
    dist_local: str
    serve: str
    ssh_config: str
    helpers: list[HostHelper]
    solo_first: bool = dataclasses.field(default=True, kw_only=True)


def load_host(raw: dict) -> HostController | HostWorker:
    if raw.get("role") == "controller":
        evidence.require(isinstance(raw.get("helpers"), list) and len(raw["helpers"]) == 2,
                         "controller requires exactly two SSH helpers")
        given = HostController(**{**raw, "helpers": [HostHelper(**value) for value in raw["helpers"]]})
        evidence.require(type(given.solo_first) is bool, "solo_first must be a boolean")
        evidence.require([helper.alias for helper in given.helpers] == ["helper-1", "helper-2"],
                         "helpers must use the pinned helper-1 and helper-2 SSH aliases")
        for name in ("dist_local", "serve", "ssh_config"):
            host_absolute(getattr(given, name), name)
        for node in given.helpers:
            host_valid_node(node)
    else:
        given = HostWorker(**raw)
        evidence.require(given.role in ("primary", "helper"), "role must be controller, primary or helper")
    host_valid_node(given)
    evidence.require(type(given.benchmark) is bool, "benchmark must be a boolean")
    evidence.require(type(given.cores) is int and given.cores > 0 and type(given.timeout) is int
                     and 0 < given.timeout <= 3600, "cores must be positive and timeout must be 1–3600 seconds")
    evidence.require(isinstance(given.namespace, str) and bool(given.namespace),
                     "namespace must identify this rendezvous")
    return given


def host_absolute(value: str, name: str) -> None:
    evidence.require(isinstance(value, str) and "\0" not in value and Path(value).is_absolute(),
                     f"{name} must be an absolute path")


def host_valid_node(node: HostNode) -> None:
    for name in ("checkout", "scratch", "blaze", "python", "socket"):
        host_absolute(getattr(node, name), name)
    evidence.require(isinstance(node.runner, str) and bool(node.runner), "runner identity is required")
    host_cache_address(node.blobs)


def host_cache_address(value: str) -> tuple[str, int]:
    evidence.require(isinstance(value, str), "cache address must be a string")
    host, separator, port = value.partition(":")
    evidence.require(host == "127.0.0.1" and bool(separator) and port.isdecimal() and 0 < int(port) <= 65535,
                     f"cache listener must use a numeric loopback address: {value!r}")
    return host, int(port)


def host_write_json(path: Path, value: object) -> None:
    path.write_text(json.dumps(value, indent=2) + "\n")


def host_send(stream, message: dict) -> None:
    stream.write(json.dumps(message, separators=(",", ":")).encode() + b"\n")
    stream.flush()


class HostMessages:
    """Bounded JSON lines over one connected SSH or local subprocess stream."""

    def __init__(self, stream, label: str):
        self.stream = stream
        self.label = label
        self.buffer = bytearray()

    def receive(self, phase: str, deadline: float) -> dict:
        with DefaultSelector() as selector:
            selector.register(self.stream, EVENT_READ)
            while b"\n" not in self.buffer:
                remaining = deadline - time.monotonic()
                evidence.require(remaining > 0 and bool(selector.select(remaining)),
                                 f"{self.label}: deadline waiting for {phase}")
                chunk = os.read(self.stream.fileno(), 65536)
                evidence.require(bool(chunk), f"{self.label}: stream ended before {phase}")
                self.buffer.extend(chunk)
                evidence.require(len(self.buffer) <= HOST_MESSAGE_LIMIT, f"{self.label}: message exceeds configured limit")
        row, _, rest = self.buffer.partition(b"\n")
        self.buffer = rest
        message = json.loads(row)
        evidence.require(isinstance(message, dict) and message.get("phase") == phase,
                         f"{self.label}: expected {phase}, saw {message.get('phase') if isinstance(message, dict) else type(message)}")
        return message


def host_stop(process: subprocess.Popen) -> None:
    if process.poll() is None:
        try:
            os.killpg(process.pid, signal.SIGTERM)
        except ProcessLookupError:
            pass
        try:
            process.wait(timeout=5)
        except subprocess.TimeoutExpired:
            os.killpg(process.pid, signal.SIGKILL)
            process.wait(timeout=5)
    for stream in (process.stdin, process.stdout):
        if stream is not None:
            stream.close()


def host_leaf(index: int, salt: int) -> str:
    return (
        f"#[inline(never)]\nfn leaf_{index}(n: u64) -> u64 {{ "
        f"n.wrapping_mul({2 * index + 1}).rotate_left({index % 64}) ^ {salt} }}\n"
    )


def host_leaf_salt(index: int) -> int:
    return (HOST_SEED * (2 * index + 1)) & HOST_MASK


def host_fixture_stdout(*, edited: bool) -> str:
    expected = HOST_SEED
    for index in range(HOST_LEAVES):
        salt = host_leaf_salt(index)
        if edited and index == HOST_EDIT_LEAF:
            salt ^= 1
        shift = index % 64
        term = HOST_PROGRAM_INPUT * (2 * index + 1) & HOST_MASK
        term = ((term << shift) | (term >> (64 - shift))) & HOST_MASK
        expected = ((expected ^ term ^ salt) * HOST_FNV) & HOST_MASK
    return f"{expected:016x}\n"


def host_fixture_digest(path: Path) -> str:
    identity = hashlib.sha256()
    for name in ("Cargo.lock", "Cargo.toml", "src/main.rs"):
        identity.update(name.encode() + b"\0" + bytes.fromhex(file_hash(path / name)))
    return identity.hexdigest()


def host_fixture(path: Path) -> tuple[str, str]:
    (path / "src").mkdir(parents=True)
    files = {
        "Cargo.toml": f'[package]\nname = "{HOST_PACKAGE}"\nversion = "0.1.0"\nedition = "2024"\n',
        "Cargo.lock": f'version = 4\n\n[[package]]\nname = "{HOST_PACKAGE}"\nversion = "0.1.0"\n',
    }
    source = [host_leaf(index, host_leaf_salt(index)) for index in range(HOST_LEAVES)]
    for start in range(0, HOST_LEAVES, 16):
        source.append(f"#[inline(never)]\nfn group_{start}(n: u64, mut hash: u64) -> u64 {{\n")
        for index in range(start, start + 16):
            source.append(f"    hash = (hash ^ leaf_{index}(n)).wrapping_mul({HOST_FNV});\n")
        source.append("    hash\n}\n")
    source.extend((
        "fn main() {\n",
        "    let n: u64 = std::env::args().nth(1).unwrap().parse().unwrap();\n",
        f"    let mut hash: u64 = {HOST_SEED};\n",
    ))
    for index in range(0, HOST_LEAVES, 16):
        source.append(f"    hash = group_{index}(n, hash);\n")
    source.append('    println!("{hash:016x}");\n}\n')
    files["src/main.rs"] = "".join(source)
    for name, data in files.items():
        (path / name).write_text(data)
    return host_fixture_digest(path), host_fixture_stdout(edited=False)


def host_edit_fixture(path: Path) -> tuple[str, str]:
    source = path / "src/main.rs"
    before = source.read_text()
    salt = host_leaf_salt(HOST_EDIT_LEAF)
    old = host_leaf(HOST_EDIT_LEAF, salt)
    evidence.require(before.count(old) == 1, f"{source}: expected one unedited leaf_{HOST_EDIT_LEAF} body")
    source.write_text(before.replace(old, host_leaf(HOST_EDIT_LEAF, salt ^ 1), 1))
    return host_fixture_digest(path), host_fixture_stdout(edited=True)


def host_worker_env(root: Path, cores: int) -> dict:
    # Give every export its own persisted state. Only the compiler's staged shelf is copied.
    shelf = Path.home() / ".blaze" / "v2" / "toolchain"
    evidence.require(shelf.is_dir(), f"staged toolchain shelf is missing: {shelf}")
    home = root / "home"
    shutil.copytree(shelf, home / ".blaze" / "v2" / "toolchain")
    (root / "tmp").mkdir()
    env = {key: value for key, value in os.environ.items() if not key.startswith(("CARGO", "RUST", "BLAZE_"))}
    return {**env, "HOME": str(home), "TMPDIR": str(root / "tmp"), "XDG_CACHE_HOME": str(root / "cache"),
            "BLAZE_CORES": str(cores), "CARGO_TARGET_DIR": str(root / "target"), "RUST_BACKTRACE": "0"}


def host_checked_output(command: list[str], *, timeout: int, env: dict | None = None) -> str:
    return subprocess.check_output(command, text=True, timeout=timeout, env=env, stdin=subprocess.DEVNULL)


def host_affinity() -> list[int]:
    evidence.require(sys.platform.startswith("linux") and callable(getattr(os, "sched_getaffinity", None))
                     and callable(getattr(os, "sched_setaffinity", None)),
                     "three-host benchmark requires Linux CPU affinity")
    try:
        return sorted(os.sched_getaffinity(0))
    except OSError as error:
        raise evidence.Failed(f"cannot read benchmark CPU affinity: {error}") from error


def host_pin_cores(cores: int) -> None:
    # Called at session entry, before threads or services: fork and exec inherit this mask.
    available = host_affinity()
    evidence.require(len(available) >= cores,
                     f"benchmark needs {cores} CPUs in the runner affinity; available {available}")
    selected = available[:cores]
    try:
        os.sched_setaffinity(0, selected)
    except OSError as error:
        raise evidence.Failed(f"cannot pin benchmark CPU affinity to {selected}: {error}") from error
    evidence.require(host_affinity() == selected, f"kernel did not retain benchmark CPU affinity {selected}")


def host_cpu_list(raw: str) -> set[int]:
    evidence.require(re.fullmatch(r"[0-9]+(?:-[0-9]+)?(?:,[0-9]+(?:-[0-9]+)?)*", raw) is not None,
                     f"invalid effective cgroup cpuset: {raw!r}")
    cpus = set()
    for part in raw.split(","):
        first, _, last = part.partition("-")
        start, end = int(first), int(last or first)
        evidence.require(start <= end, f"reversed effective cgroup cpuset range: {part}")
        cpus.update(range(start, end + 1))
    return cpus


def host_cgroup_budget(cores: int, cpus: list[int], *, proc: Path = Path("/proc/self")) -> dict:
    """Read every visible ancestor limit; a leaf's unlimited cpu.max does not remove its parent's cap."""
    try:
        membership = (proc / "cgroup").read_text().strip()
        groups = [line.removeprefix("0::") for line in membership.splitlines() if line.startswith("0::")]
        evidence.require(len(groups) == 1 and groups[0].startswith("/") and ".." not in Path(groups[0]).parts,
                         "benchmark requires a readable cgroup v2 membership")
        mounts = [line.split(" - ", 1)[0].split() for line in (proc / "mountinfo").read_text().splitlines()
                  if " - cgroup2 " in line]
        evidence.require(len(mounts) == 1 and len(mounts[0]) >= 5 and mounts[0][3] == "/",
                         "benchmark requires the full cgroup v2 hierarchy to check ancestor quotas")
        mount = re.sub(r"\\([0-7]{3})", lambda match: chr(int(match[1], 8)), mounts[0][4])
        root = Path(mount)
        controllers = (root / "cgroup.controllers").read_text().split()
        evidence.require({"cpu", "cpuset"}.issubset(controllers),
                         "benchmark requires readable cgroup v2 CPU and cpuset controllers")
        current = root / groups[0].removeprefix("/")
        evidence.require(root.is_absolute() and current.is_dir(), "benchmark cgroup membership is not visible at its mount")
        limits = []
        effective_cpuset = None
        effective_quota = None
        while True:
            row = {"path": str(current)}
            for name in ("cpu.max", "cpuset.cpus.effective"):
                try:
                    row[name] = (current / name).read_text().strip()
                except FileNotFoundError:
                    # The root has no cpu.max; a controller not enabled below an ancestor
                    # adds no child limit. Keep those absences in the retained evidence.
                    row[name] = None
            limits.append(row)
            if row["cpu.max"] is not None:
                quota, period = row["cpu.max"].split()
                evidence.require(period.isdecimal() and int(period) > 0 and
                                 (quota == "max" or (quota.isdecimal() and int(quota) > 0)),
                                 f"{current}: invalid cpu.max {row['cpu.max']!r}")
                if quota != "max":
                    quota_us, period_us = int(quota), int(period)
                    evidence.require(quota_us >= cores * period_us,
                                     f"{current}: cpu.max {quota} {period} allows less than {cores} benchmark cores")
                    if (effective_quota is None or
                            quota_us * effective_quota["period_us"] < effective_quota["quota_us"] * period_us):
                        effective_quota = {"path": str(current), "quota_us": quota_us, "period_us": period_us}
            if row["cpuset.cpus.effective"] is not None:
                allowed = host_cpu_list(row["cpuset.cpus.effective"])
                evidence.require(set(cpus).issubset(allowed),
                                 f"{current}: benchmark affinity {cpus} escapes effective cgroup cpuset {sorted(allowed)}")
                if effective_cpuset is None:
                    effective_cpuset = sorted(allowed)
            if current == root:
                break
            current = current.parent
        evidence.require(effective_cpuset is not None, "benchmark cgroup hierarchy has no effective cpuset evidence")
        return {"membership": membership, "mount": mount, "controllers": controllers, "limits": limits,
                "effective_cpuset": effective_cpuset, "effective_cpu_max": effective_quota}
    except (OSError, ValueError) as error:
        raise evidence.Failed(f"cannot establish benchmark cgroup CPU budget: {error}") from error


def host_resources(cores: int) -> dict:
    cpus = host_affinity()
    evidence.require(len(cpus) == cores, f"benchmark requires exactly {cores} pinned CPUs; observed {cpus}")
    return {"configured_workers": cores, "sched_affinity": cpus, "cgroup": host_cgroup_budget(cores, cpus)}


def host_node_identity(given: HostWorker, env: dict, source: str) -> dict:
    resources = {"resources": host_resources(given.cores)} if given.benchmark else {}
    boot_id = str(UUID(Path("/proc/sys/kernel/random/boot_id").read_text().strip()))
    git = ["git", "-C", given.checkout, "rev-parse"]
    triple = json.loads(host_checked_output([given.blaze, "toolchain", "--json"], timeout=given.timeout, env=env))["host"]
    binary = Path(given.blaze)
    digest = file_hash(binary)
    seal_path = binary.with_name("blaze-build.json")
    seal = json.loads(seal_path.read_text())
    evidence.require(seal.get("schema") == 2 and seal["binaries"]["blaze"] == digest,
                     f"{binary}: compiler does not match its source-build identity")
    return {
        "namespace": given.namespace,
        "runner": given.runner,
        "boot_id": boot_id,
        "commit": host_checked_output([*git, "HEAD"], timeout=given.timeout).strip(),
        "tree": host_checked_output([*git, "HEAD^{tree}"], timeout=given.timeout).strip(),
        "blaze_sha256": digest,
        "compiler_source": seal["source"],
        "compiler_seal_sha256": file_hash(seal_path),
        "workload_sha256": source,
        "triple": triple,
        "cores": given.cores,
        **resources,
    }


def host_wait_gate(gate: Path, events_path: Path, process: subprocess.Popen, deadline: float) -> dict:
    while time.monotonic() < deadline and process.poll() is None:
        events = evidence.jsonl(events_path, live=True)
        if (gate / "ready.json").is_file():
            # Read the reported pid from this worker's own validated grant stream. The
            # actual host binding is the authenticated session plus boot ID, not this pid.
            for grant in reversed(events):
                if grant["event"] == "grant":
                    captured = evidence.held_result(gate, events, grant["pid"])
                    if captured is not None:
                        return captured
                    break
        if (gate / "error.txt").is_file():
            raise evidence.Failed((gate / "error.txt").read_text())
        time.sleep(0.02)
    raise evidence.Failed(f"no real held batch from {events_path.parent}; see export.log")


def host_spawn_export(given: HostWorker, root: Path, project: Path, env: dict, log) -> subprocess.Popen:
    if given.benchmark:
        evidence.require(len(host_affinity()) == given.cores, "benchmark CPU affinity changed before export")
    return subprocess.Popen(
        [given.blaze, "export", "-p", HOST_PACKAGE, "-o", str(root / "artifact"), "--stats"],
        cwd=project, env=env, stdin=subprocess.DEVNULL, stdout=log, stderr=subprocess.STDOUT,
        start_new_session=True,
    )


def host_run_export(given: HostWorker, root: Path, project: Path, env: dict) -> subprocess.Popen:
    with (root / "export.log").open("xb") as log:
        return host_spawn_export(given, root, project, env, log)


def host_artifact(root: Path, expected: str, timeout: int) -> dict:
    path = root / "artifact"
    output = host_checked_output([str(path), str(HOST_PROGRAM_INPUT)], timeout=timeout)
    evidence.require(output == expected, f"{path}: unexpected function-result hash {output!r}; expected {expected!r}")
    return {"stdout": output, "sha256": file_hash(path)}


def host_latency_work(log_path: Path, report_path: Path | None, start_profile: Path) -> dict:
    prefix = "export stats: "
    lines = [line.removeprefix(prefix) for line in log_path.read_text().splitlines() if line.startswith(prefix)]
    evidence.require(len(lines) == 1, f"{log_path}: expected one final export stats line")
    raw_stats = dict(field.split("=", 1) for field in lines[0].split())
    stats = {name: int(value) for name, value in raw_stats.items() if name != "wall"}
    stats["wall_ms"] = int(raw_stats["wall"].removesuffix("ms"))
    work = {"export_stats": stats}
    if report_path is None:
        return work
    rows = evidence.jsonl(report_path)
    evidence.require(bool(rows), f"{report_path}: workspace-distributed export reported no units")
    return {**work, **distributed_work(rows, start_profile)}


def distributed_work(rows: list[dict], start_profile: Path) -> dict:
    for row in rows:
        evidence.compiled(row)
        evidence.require(row["source"] in ("executed", "published"), f"{row['unit']}: unknown unit source")
    work = {name: sums(rows, name) for name in ("grants", "compiled_for_work", "stored_for_work", "tail_compiled", "from_store")}
    for source in ("executed", "published"):
        work[f"{source}_units"] = sum(row["source"] == source for row in rows)
    work["verified_reachability"] = [
        {name: row[name] for name in ("unit", "unit_digest", "source", "completed_here", "verified_reach_rows", "verified_reach_digest")}
        for row in rows if row.get("verified_reach_rows", 0) > 0
    ]
    placed = evidence.placed_verified_reach_rows(start_profile)
    if work["verified_reachability"]:
        evidence.require(placed is not None, f"{start_profile}: missing verified reachability placement counter")
    work["placed_verified_reach_rows"] = placed
    return work


def host_body_workers(profile: Path, cores: int) -> int:
    rows = re.findall(r"^Bodies floor:.*; at ([0-9]+) workers the same sum would be ",
                      profile.read_text(), re.MULTILINE)
    evidence.require(len(rows) == 1, f"{profile}: expected one measured body-pool worker count")
    observed = int(rows[0])
    evidence.require(observed == cores, f"{profile}: requested {cores} workers but compiler reported {observed}")
    return observed


def host_latency_export(given: HostWorker, root: Path, project: Path, env: dict, phase: str,
                        source: str, expected: str) -> dict:
    phase_dir = root / phase
    phase_dir.mkdir()
    log_path = phase_dir / "export.log"
    report_path = None
    if "BLAZE_DIST_SOCKET" in env:
        report_path = phase_dir / "report.jsonl"
        env = {**env, "BLAZE_DIST_REPORT": str(report_path)}
    state = project / ".blaze" / "v2"
    profiles = ("profile.txt", "start.txt")
    for name in profiles:
        (state / name).unlink(missing_ok=True)
    with log_path.open("xb") as log:
        load_before = os.getloadavg()
        started = time.monotonic_ns()
        process = host_spawn_export(given, root, project, env, log)
    try:
        code = process.wait(timeout=given.timeout)
        elapsed = time.monotonic_ns() - started
        load_after = os.getloadavg()
        evidence.require(code == 0, f"{given.runner}: {phase} export exited {code}; see {log_path}")
    finally:
        host_stop(process)
    artifact = host_artifact(root, expected, given.timeout)
    evidence.require(host_fixture_digest(project) == source, f"{project}: {phase} fixture changed during export")
    for name in profiles:
        shutil.copyfile(state / name, phase_dir / name)
    work = host_latency_work(log_path, report_path, phase_dir / "start.txt")
    work["body_pool_workers"] = host_body_workers(phase_dir / "profile.txt", given.cores)
    work_path = phase_dir / "work.json"
    host_write_json(work_path, work)
    evidence_paths = {"log": str(log_path), "work": str(work_path),
                      **{name: str(phase_dir / name) for name in profiles}}
    if report_path is not None:
        evidence_paths.update(report=str(report_path), plans=f"{report_path}.plans")
    return {
        "phase": phase, "elapsed_ns": elapsed, "source_sha256": source, "artifact": artifact,
        "work": work, "evidence": evidence_paths, "load_before": load_before, "load_after": load_after,
    }


def host_check_latency_edit(cold: dict, edited: dict) -> None:
    evidence.require(cold["source_sha256"] != edited["source_sha256"], "the private body edit did not change fixture bytes")
    for name in ("stdout", "sha256"):
        evidence.require(cold["artifact"][name] != edited["artifact"][name],
                         f"the private body edit did not change artifact {name}")


def host_latency_worker(given: HostWorker, root: Path, project: Path, env: dict, identity: dict,
                        expected: str, controls: HostMessages) -> None:
    env = {**env, "BLAZE_DIST_SOCKET": given.socket, "BLAZE_DIST_BLOBS": given.blobs,
           "BLAZE_DIST_LOCAL": "never", "BLAZE_DIST_DEADLINE_SECS": str(given.timeout)}
    host_send(sys.stdout.buffer, {"phase": "prepared", "identity": identity})
    source = identity["workload_sha256"]
    previous = None
    for phase in ("cold", "edit"):
        controls.receive(phase, time.monotonic() + given.timeout)
        result = host_latency_export(given, root, project, env, phase, source, expected)
        measured = host_node_identity(given, env, source)
        evidence.require(measured == {**identity, "workload_sha256": source},
                         f"{given.runner}: measured identity changed during {phase}")
        result["identity"] = {**measured, "observed_body_workers": result["work"]["body_pool_workers"]}
        if previous is not None:
            host_check_latency_edit(previous, result)
        host_write_json(root / phase / "result.json", result)
        host_send(sys.stdout.buffer, result)
        if phase == "cold":
            previous = result
            source, expected = host_edit_fixture(project)
            host_send(sys.stdout.buffer, {"phase": "edit_ready", "source_sha256": source, "expected_stdout": expected})


def host_worker(given: HostWorker) -> None:
    if given.benchmark:
        host_pin_cores(given.cores)
    root = Path(given.scratch)
    root.mkdir(parents=True, exist_ok=False)
    project = root / "project"
    source, expected = host_fixture(project)
    env = host_worker_env(root, given.cores)
    identity = host_node_identity(given, env, source)
    host_write_json(root / "identity.json", identity)
    controls = HostMessages(sys.stdin.buffer, "controller")
    if given.benchmark:
        host_latency_worker(given, root, project, env, identity, expected, controls)
        return
    host_send(sys.stdout.buffer, {"phase": "prepared", "identity": identity})
    controls.receive("start", time.monotonic() + given.timeout)
    gate = root / "gate"
    gate.mkdir()
    (gate / "keepalive").touch()
    events_path = root / "events.jsonl"
    report_path = root / "report.jsonl"
    artifact_keys_path = root / "artifact-keys.json"
    env.update(BLAZE_DIST_SOCKET=given.socket, BLAZE_DIST_BLOBS=given.blobs,
               BLAZE_DIST_REPORT=str(report_path), BLAZE_DIST_EVENTS=str(events_path),
               BLAZE_DIST_DEADLINE_SECS=str(given.timeout),
               BLAZE_DIST_TEST_RESULT_GATE=str(gate), BLAZE_DIST_TEST_RESULT_UNIT=HOST_PACKAGE,
               BLAZE_DIST_TEST_RESULT_SCOPE="all")
    if given.role == "primary":
        env["BLAZE_DIST_ARTIFACT_KEYS"] = str(artifact_keys_path)
    started = time.monotonic_ns()
    process = host_run_export(given, root, project, env)
    try:
        deadline = time.monotonic() + given.timeout
        held = host_wait_gate(gate, events_path, process, deadline)
        capture = json.loads((gate / "ready.json").read_text())
        host_send(sys.stdout.buffer, {"phase": "held", "grant": held, "capture": capture})
        controls.receive("release", deadline)
        (gate / "release").touch()
        code = process.wait(timeout=max(0.001, deadline - time.monotonic()))
        elapsed = time.monotonic_ns() - started
        evidence.require(code == 0, f"{given.runner}: export exited {code}; see {root / 'export.log'}")
        evidence.require(evidence.gate_answer(gate, held, refused=False),
                         f"{given.runner}: the actual Client did not retain the accepted Result answer")
        evidence.require(host_node_identity(given, env, source) == identity, f"{given.runner}: measured identity changed")
        completed = {"phase": "complete", "elapsed_ns": elapsed, "identity": identity,
                     "artifact": host_artifact(root, expected, given.timeout), "rows": evidence.jsonl(report_path),
                     "plans": evidence.unit_plan(Path(str(report_path) + ".plans"))}
        if given.role == "primary":
            evidence.no_local_emission("primary final assembly", (root / "export.log").read_text())
            completed["artifact_keys"] = json.loads(artifact_keys_path.read_text())
        host_write_json(root / "completion.json", completed)
        host_send(sys.stdout.buffer, completed)
    finally:
        host_stop(process)


def host_same_hosts(identities: list[dict]) -> None:
    evidence.require(len(identities) == 3, "exactly three sessions must report identity")
    for field in ("namespace", "commit", "tree", "blaze_sha256", "compiler_seal_sha256",
                  "workload_sha256", "triple", "cores"):
        evidence.require(len({identity[field] for identity in identities}) == 1, f"participants disagree on {field}")
    evidence.require(len({identity["boot_id"] for identity in identities}) == 3,
                     "participants must have distinct boot_id values")


def host_shared_grants(held: list[dict]) -> tuple[str, int, int]:
    grants = [row["grant"] for row in held]
    evidence.validate_events(grants)
    executions = {(grant["unit_digest"], grant["execution"], grant["attempt"]) for grant in grants}
    evidence.require(len(grants) == 3 and len(executions) == 1 and len({grant["holder"] for grant in grants}) == 3,
                     "three distinct holders must share one unit, execution and attempt")
    used: set[str] = set()
    for grant in grants:
        work = set(grant["work"])
        evidence.require(bool(work) and used.isdisjoint(work), "every participant must have disjoint nonempty work")
        used.update(work)
    return next(iter(executions))


def host_dev_machine_entry(record: bytes) -> bool:
    """Dev packs contain ladder entries, with the machine record embedded in the entry."""
    if len(record) < 20 or not record.startswith(b"bzcent01"):
        return False
    form, _, size = struct.unpack_from("<III", record, 8)
    return form == 9 and size <= len(record) - 20 and evidence.machine_record(record[20:20 + size])


def host_accepted_outputs(blobs: Path, held: dict, participant: dict, events: list[dict], unit: str,
                     published: dict[str, tuple[int, bytes]]) -> set[str]:
    grant, capture = held["grant"], held["capture"]
    expected_job = evidence.main_job_content(unit, participant["identity"]["triple"])
    evidence.require(capture["command"][1]["0"] == expected_job, f"{grant['holder']}: captured Result names a different job")
    evidence.require(capture["command_sha256"] == grant["capture_sha256"], f"{grant['holder']}: captured Result digest changed")
    acknowledgements = [event for event in events if event["event"] == "result_accepted"
                        and evidence.lease(event) == evidence.lease(grant)
                        and set(event["work"]) == set(grant["work"])]
    evidence.require(bool(acknowledgements), f"coordinator did not accept {grant['holder']}'s exact held batch")
    references = capture["command"][1]["2"]["3"]
    batch = evidence.logical_outputs(blobs, [reference["0"] for reference in references])
    rows = [row for row in participant["rows"] if row["unit_digest"] == unit]
    evidence.require(len(rows) == 1 and rows[0]["joined"] and rows[0]["source"] == "executed"
                     and rows[0]["execution"] == grant["execution"] and rows[0]["attempt"] == grant["attempt"],
                     f"{grant['holder']}: unit report does not identify its shared execution")
    evidence.compiled(rows[0])
    compiled = {key for key in rows[0]["work_compiled_keys"]
                if key in batch and batch[key][0] == 0 and host_dev_machine_entry(batch[key][1])}
    evidence.require(bool(compiled), f"{grant['holder']}: accepted batch contains no locally compiled function keys")
    evidence.require(all(published.get(key) == batch[key] for key in compiled),
                     f"{grant['holder']}: published bytes differ from its accepted compiled records")
    return compiled


def host_placed_helper_keys(primary: dict, helpers: list[set[str]]) -> list[list[str]]:
    receipt = primary["artifact_keys"]
    evidence.require(isinstance(receipt, dict) and receipt.get("artifact_sha256") == primary["artifact"]["sha256"],
                     "primary placement receipt must bind the final artifact bytes")
    placed = receipt.get("keys")
    evidence.require(isinstance(placed, list) and all(evidence.digest(key) for key in placed)
                     and placed == sorted(set(placed)), "primary placement receipt needs sorted unique function keys")
    consumed = [sorted(keys.intersection(placed)) for keys in helpers]
    evidence.require(len(consumed) == 2 and all(consumed), "primary must place accepted function records from both helpers")
    return consumed


def host_prove(blobs: Path, held: list[dict], completed: list[dict], events: list[dict], solo: dict) -> dict:
    unit, execution, attempt = host_shared_grants(held)
    for participant in completed:
        evidence.require(participant["artifact"] == solo, "every worker's artifact must match solo in bytes and stdout")
        evidence.same_unit_plans(completed[0]["plans"], participant["plans"])
    all_rows = [row for participant in completed for row in participant["rows"]]
    evidence.no_duplicate_compile(all_rows)
    evidence.publication(blobs, all_rows, events)
    packs, _ = evidence.manifest(blobs, unit)
    published = evidence.logical_outputs(blobs, packs)
    outputs = [host_accepted_outputs(blobs, gate, participant, events, unit, published)
               for gate, participant in zip(held, completed, strict=True)]
    for index, keys in enumerate(outputs):
        evidence.require(all(keys.isdisjoint(other) for other in outputs[index + 1:]),
                         "accepted compiled function sets overlap")
    consumed = host_placed_helper_keys(completed[0], outputs[1:])
    return {"unit_digest": unit, "execution": execution, "attempt": attempt,
            "grants": [row["grant"] for row in held], "accepted_compiled_keys": [sorted(keys) for keys in outputs],
            "placed_helper_keys": consumed}


def host_wait_ready(process: subprocess.Popen, predicate, deadline: float, description: str) -> None:
    while process.poll() is None and time.monotonic() < deadline:
        if predicate():
            return
        time.sleep(0.02)
    raise evidence.Failed(f"{description} did not start")


def host_connected(address: str) -> bool:
    try:
        with socket.create_connection(host_cache_address(address), timeout=0.2):
            return True
    except OSError:
        return False


def host_solo_latency(given: HostController, root: Path, project: Path, env: dict,
                      identity: dict, expected: str) -> dict:
    cold = host_latency_export(given, root, project, env, "cold", identity["workload_sha256"], expected)
    source, edited_stdout = host_edit_fixture(project)
    edit = host_latency_export(given, root, project, env, "edit", source, edited_stdout)
    edited_identity = host_node_identity(given, env, source)
    evidence.require(edited_identity == {**identity, "workload_sha256": source},
                     "solo compiler or host identity changed during timing")
    host_check_latency_edit(cold, edit)
    return {"cold": {**cold, "identity": {**identity, "observed_body_workers": cold["work"]["body_pool_workers"]}},
            "edit": {**edit, "identity": {**edited_identity, "observed_body_workers": edit["work"]["body_pool_workers"]}}}


def host_distributed_latency(given: HostController, channels: list, identities: list[dict]) -> dict:
    def export(phase: str) -> list[dict]:
        deadline = time.monotonic() + given.timeout
        # SSH carries the helper controls. Queue them before releasing the local primary.
        for stream, _, _ in (*channels[1:], channels[0]):
            host_send(stream, {"phase": phase})
        return [channel.receive(phase, deadline) for _, channel, _ in channels]

    cold = export("cold")
    deadline = time.monotonic() + given.timeout
    edits = [channel.receive("edit_ready", deadline) for _, channel, _ in channels]
    source = edits[0]["source_sha256"]
    stdout = edits[0]["expected_stdout"]
    evidence.require(all(row["source_sha256"] == source and row["expected_stdout"] == stdout for row in edits),
                     "participants prepared different private-body edits")
    edit = export("edit")
    deadline = time.monotonic() + given.timeout
    for (_, _, child), identity, before, after in zip(channels, identities, cold, edit, strict=True):
        evidence.require(child.wait(timeout=max(0.001, deadline - time.monotonic())) == 0,
                         f"{identity['runner']}: worker session failed")
        expected = {**identity, "observed_body_workers": given.cores}
        evidence.require(before["identity"] == expected and before["source_sha256"] == identity["workload_sha256"]
                         and after["identity"] == {**expected, "workload_sha256": source}
                         and after["source_sha256"] == source and after["artifact"]["stdout"] == stdout,
                         f"{identity['runner']}: measured host or source differs from the prepared worker")
        host_check_latency_edit(before, after)
    return {"cold": cold, "edit": edit}


def host_latency_report(root: Path, given: HostController, identities: list[dict], solo: dict,
                        distributed: dict) -> None:
    roles = ["primary", *(helper.alias for helper in given.helpers)]
    order = ["solo", "distributed"] if given.solo_first else ["distributed", "solo"]
    phases = {}
    for phase, reference in solo.items():
        workers = dict(zip(roles, distributed[phase], strict=True))
        evidence.require(workers["primary"]["identity"] == reference["identity"],
                         f"{phase}: solo and distributed primary must use the same host and compiler")
        for role, result in (("solo", reference), *workers.items()):
            identity = result["identity"]
            evidence.require(identity["observed_body_workers"] == given.cores
                             and identity["resources"]["configured_workers"] == given.cores
                             and len(identity["resources"]["sched_affinity"]) == given.cores,
                             f"{phase} {role}: configured, pinned and observed workers must match the core budget")
            evidence.require(result["source_sha256"] == reference["source_sha256"]
                             and result["artifact"] == reference["artifact"],
                             f"{phase} {role}: source, artifact bytes and stdout must match the solo control")
        phases[phase] = {"solo_export_ns": reference["elapsed_ns"],
                         "primary_export_ns": workers["primary"]["elapsed_ns"],
                         "solo": reference, "workers": workers}
    host_write_json(root / "three-host-latency.json", {
        "schema": 2, "order": order, "hosts": dict(zip(roles, identities, strict=True)), "phases": phases,
        "timing": "Per-process monotonic wall time on the launching host from export spawn through exit",
    })
    print("distv2: unpaused cold and private-body edit exports match solo on all three hosts", flush=True)


def host_controller(given: HostController) -> None:
    if given.benchmark:
        host_pin_cores(given.cores)
    root = Path(given.scratch)
    root.mkdir(parents=True, exist_ok=False)
    solo_root = root / "solo"
    solo_root.mkdir()
    project = solo_root / "project"
    solo_source, expected = host_fixture(project)
    env = host_worker_env(solo_root, given.cores)
    if given.benchmark:
        solo_identity = host_node_identity(given, env, solo_source)
        triple = solo_identity["triple"]
        if given.solo_first:
            solo = host_solo_latency(given, solo_root, project, env, solo_identity, expected)
    else:
        started = time.monotonic_ns()
        process = host_run_export(given, solo_root, project, env)
        try:
            code = process.wait(timeout=given.timeout)
            solo_ns = time.monotonic_ns() - started
            evidence.require(code == 0, f"solo export exited {code}; see {solo_root / 'export.log'}")
        finally:
            host_stop(process)
        solo = host_artifact(solo_root, expected, given.timeout)
        triple = json.loads(host_checked_output([given.blaze, "toolchain", "--json"], timeout=given.timeout, env=env))["host"]

    services = []
    channels = []
    blobs = root / "blobs"
    coordinator_events = root / "coordinator.events.jsonl"
    carrier = Path(given.socket)
    evidence.require(not carrier.exists() and not carrier.is_symlink(), f"coordinator socket already exists: {carrier}")
    coordinator_started = False
    try:
        with (root / "serve.log").open("xb") as log:
            cache = subprocess.Popen([given.serve, "--dir", str(blobs), "--listen", given.blobs],
                                     stdin=subprocess.DEVNULL, stdout=log, stderr=subprocess.STDOUT, start_new_session=True)
        services.append(cache)
        host_wait_ready(cache, lambda: host_connected(given.blobs), time.monotonic() + 30, "native cache")
        coordinator_env = env if given.benchmark else {**env, "BLAZE_DIST_EVENTS": str(coordinator_events)}
        with (root / "coordinator.log").open("xb") as log:
            coord = subprocess.Popen([given.dist_local, "coordinate", given.socket, triple, "64"],
                                     env=coordinator_env,
                                     stdin=subprocess.PIPE, stdout=log, stderr=subprocess.STDOUT, start_new_session=True)
        services.append(coord)
        coordinator_started = True
        host_wait_ready(coord, carrier.is_socket, time.monotonic() + 30, "coordinator UDS")
        primary = HostNode(given.checkout, str(root / "worker"), given.blaze, given.python, given.runner, given.socket, given.blobs)
        for index, node in enumerate((primary, *given.helpers)):
            role = "primary" if index == 0 else "helper"
            fields = {field.name: getattr(node, field.name) for field in dataclasses.fields(HostNode)}
            request = HostWorker(**fields, role=role, namespace=given.namespace, cores=given.cores,
                                 timeout=given.timeout, benchmark=given.benchmark)
            command = [node.python, str(Path(node.checkout) / "harness" / "distv2.py"),
                       json.dumps(dataclasses.asdict(request), separators=(",", ":"))]
            if isinstance(node, HostHelper):
                command = ["ssh", "-F", given.ssh_config, node.alias, shlex.join(command)]
            with (root / f"session-{index}.log").open("xb") as log:
                child = subprocess.Popen(command, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=log,
                                         start_new_session=True)
            services.append(child)
            channels.append((child.stdin, HostMessages(child.stdout, f"{role} {node.runner}"), child))

        deadline = time.monotonic() + given.timeout
        identities = [channel.receive("prepared", deadline)["identity"] for _, channel, _ in channels]
        host_same_hosts(identities)
        evidence.require(identities[0]["workload_sha256"] == solo_source,
                         "the solo and distributed workloads must have identical source")
        host_write_json(root / "hosts.json", identities)
        print("Three distinct runners prepared matching source, compiler and fixture identities", flush=True)
        if given.benchmark:
            distributed = host_distributed_latency(given, channels, identities)
            if not given.solo_first:
                solo = host_solo_latency(given, solo_root, project, env, solo_identity, expected)
            host_latency_report(root, given, identities, solo, distributed)
            return

        deadline = time.monotonic() + given.timeout
        host_send(channels[0][0], {"phase": "start"})
        held = [channels[0][1].receive("held", deadline)]
        for stream, _, _ in channels[1:]:
            host_send(stream, {"phase": "start"})
        held.extend(channel.receive("held", deadline) for _, channel, _ in channels[1:])
        host_shared_grants(held)
        host_write_json(root / "held.json", held)
        print("Three nonempty, disjoint grants reached the explicit correctness barrier", flush=True)
        for stream, _, _ in channels:
            host_send(stream, {"phase": "release"})
        completed = [channel.receive("complete", deadline) for _, channel, _ in channels]
        for (_, _, child), identity, worker_result in zip(channels, identities, completed, strict=True):
            evidence.require(worker_result["identity"] == identity and child.wait(timeout=max(0.001, deadline - time.monotonic())) == 0,
                             f"{identity['runner']}: identity changed or its session failed")
        events = evidence.jsonl(coordinator_events)
        evidence.validate_events(events)
        proof = host_prove(blobs, held, completed, events, solo)
        host_write_json(root / "three-host-proof.json", {
            "hosts": identities, "solo_artifact": solo, "solo_export_ns": solo_ns,
            "gated_primary_export_ns": completed[0]["elapsed_ns"],
            "gated_worker_export_ns": [row["elapsed_ns"] for row in completed],
            "barrier": "primary holds its first batch, then both helpers hold their first batches before all release",
            "proof": proof,
        })
        print("distv2: three-host cooperation and primary artifact checks passed", flush=True)
    finally:
        for process in reversed(services):
            host_stop(process)
        if coordinator_started and carrier.is_socket():
            carrier.unlink()


if __name__ == "__main__":
    if len(sys.argv) != 2:
        sys.exit(__doc__)
    try:
        request = read_input(sys.argv[1])
        if "role" in request:
            signal.signal(signal.SIGTERM, lambda signum, _: sys.exit(f"distv2: interrupted by signal {signum}"))
            hosts = load_host(request)
            if isinstance(hosts, HostController):
                host_controller(hosts)
            else:
                host_worker(hosts)
        else:
            main(load_profile(request))
    except (Failed, OSError, ValueError, TypeError, KeyError, subprocess.SubprocessError) as failed:
        sys.exit(f"distv2: {failed}")
