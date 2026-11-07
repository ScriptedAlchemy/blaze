#!/usr/bin/env python3
"""distv2.py: a real V2 build of a workspace across two real daemons, and its cache.

Input, one JSON value (`python3 harness/distv2.py '{...}'`):
    blaze        the blaze binary under test
    dist_local   the `blaze-dist-local` binary (coordinator)
    serve        the `blaze-serve` binary (cache server)
    scratch      a directory for homes, stores, sockets and logs; it is removed first
    project      (optional) the workspace to build; default harness/distv2/fixture
    args         (optional) arguments of `blaze build`
    program      (optional) the path of the program, relative to its build directory's debug/;
                 default "app"
    run          (optional) the arguments it is run with; default ["11"]
    faults       (optional) run the worker-loss and carrier-loss cases; default true

Checks, each on what the build itself reports (BLAZE_DIST_REPORT) and on running the program:
    solo         one daemon, no coordinator: the program the others are held to
    two workers  two daemons build at once: every unit done once by agreement, both compile, both
                 programs behave as solo's, and neither compiles anything nothing committed
    cold cache   a third daemon with an empty store takes published units and compiles none
    warm         a build with nothing changed reports no execution
    edit         one edited function: only what it reaches is worked again
    worker lost  a daemon killed in the middle: the other finishes alone, same program
    no carrier   with no coordinator the build stops naming it; `fallback` compiles here
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


def load(text: str) -> Input:
    raw = json.loads(text)
    for name in ("args", "run"):
        if name in raw:
            raw[name] = tuple(raw[name])
    return Input(**raw)


class Failed(Exception):
    pass


def check(ok: bool, what: str) -> None:
    print(("ok   " if ok else "FAIL ") + what, flush=True)
    if not ok:
        raise Failed(what)


def free_port() -> int:
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


def host_triple(blaze: str, env: dict) -> str:
    out = subprocess.run([blaze, "toolchain", "--json"], env=env, capture_output=True, text=True, check=True)
    return json.loads(out.stdout)["host"]


class Run:
    def __init__(self, given: Input):
        self.given = given
        self.root = Path(given.scratch)
        shutil.rmtree(self.root, ignore_errors=True)
        (self.root / "logs").mkdir(parents=True)
        self.socket = Path("/tmp") / f"blzd-{os.getpid()}.sock"
        self.port = free_port()
        self.daemons: list[Path] = []
        self.services: list[subprocess.Popen] = []
        self.sequence = 0

    def env(self, home: str, dist: bool, **extra) -> dict:
        env = {k: v for k, v in os.environ.items() if not k.startswith(("CARGO", "RUST", "BLAZE_"))}
        env.update(BLAZE_HOME=str(self.root / home), TMPDIR=str(self.root / "tmp"), RUST_BACKTRACE="0")
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
        return [json.loads(line) for line in path.read_text().splitlines()] if path.exists() else []

    def start_services(self) -> None:
        log = open(self.root / "logs" / "serve.log", "w")
        self.services.append(
            subprocess.Popen(
                [self.given.serve, "--dir", str(self.root / "blobs"), "--listen", f"127.0.0.1:{self.port}"],
                stdout=log,
                stderr=subprocess.STDOUT,
            )
        )
        self.start_coordinator()
        deadline = time.time() + 30
        while time.time() < deadline:
            try:
                socket.create_connection(("127.0.0.1", self.port), timeout=1).close()
                break
            except OSError:
                time.sleep(0.05)
        else:
            raise Failed("the cache server did not listen")

    def start_coordinator(self) -> None:
        triple = host_triple(self.given.blaze, self.env("solo", False))
        log = open(self.root / "logs" / "coordinator.log", "a")
        self.socket.unlink(missing_ok=True)
        coordinator = subprocess.Popen(
            [self.given.dist_local, "coordinate", str(self.socket), triple, "64"],
            stdin=subprocess.PIPE,
            stdout=log,
            stderr=subprocess.STDOUT,
        )
        self.coordinator = coordinator
        self.services.append(coordinator)
        deadline = time.time() + 30
        while not self.socket.exists() and time.time() < deadline:
            time.sleep(0.02)
        if not self.socket.exists():
            raise Failed("the coordinator did not bind")

    def build(self, home: str, dist: bool = True, wait: bool = True, **extra):
        self.sequence += 1
        log = open(self.root / "logs" / f"{home}.{self.sequence}.log", "w")
        process = subprocess.Popen(
            [self.given.blaze, "build", *self.given.args],
            cwd=self.project,
            env=self.env(home, dist, **extra),
            stdout=log,
            stderr=subprocess.STDOUT,
        )
        if home not in self.daemons:
            self.daemons.append(home)
        if not wait:
            return process, log.name
        code = process.wait()
        return code, Path(log.name).read_text()

    def program(self, home: str) -> Path:
        out = subprocess.run(
            [self.given.blaze, "metadata", "--no-deps", "--format-version", "1"],
            cwd=self.project,
            env=self.env(home, False),
            capture_output=True,
            text=True,
        )
        return Path(json.loads(out.stdout)["target_directory"]) / "debug" / self.given.program

    def behaviour(self, home: str) -> tuple[str, str]:
        program = self.program(home)
        ran = subprocess.run([str(program), *self.given.run], capture_output=True, text=True, check=True)
        return ran.stdout, hashlib.sha256(program.read_bytes()).hexdigest()

    def stop(self, homes: list[str]) -> None:
        for home in homes:
            subprocess.run(
                [self.given.blaze, "stop"], cwd=self.project, env=self.env(home, False), capture_output=True
            )
        for service in self.services:
            if service.poll() is None:
                service.terminate()
                try:
                    service.wait(10)
                except subprocess.TimeoutExpired:
                    service.kill()
        self.socket.unlink(missing_ok=True)


def daemon_pid(run: Run, home: str) -> int | None:
    out = subprocess.run(
        [run.given.blaze, "status", "--json"], cwd=run.project, env=run.env(home, False), capture_output=True, text=True
    )
    try:
        return json.loads(out.stdout).get("pid")
    except json.JSONDecodeError:
        return None


def sums(rows: list[dict], name: str) -> int:
    return sum(row[name] for row in rows)


def main(given: Input) -> None:
    run = Run(given)
    run.project = run.root / "project"
    shutil.copytree(given.project, run.project, ignore=shutil.ignore_patterns("target", ".git"))
    homes = ["solo", "a", "b", "cold"]
    try:
        run.start_services()
        code, text = run.build("solo", dist=False)
        check(code == 0, f"solo builds: {text[-400:] if code else ''}")
        solo = run.behaviour("solo")
        print("     solo:", solo[0].strip(), solo[1][:16], flush=True)

        a, a_log = run.build("a", wait=False)
        b, b_log = run.build("b", wait=False)
        code_a, code_b = a.wait(), b.wait()
        check(code_a == 0, f"worker a builds: {Path(a_log).read_text()[-800:] if code_a else ''}")
        check(code_b == 0, f"worker b builds: {Path(b_log).read_text()[-800:] if code_b else ''}")
        rows_a, rows_b = run.report("a"), run.report("b")
        check(bool(rows_a) and bool(rows_b), f"both report units: {len(rows_a)} and {len(rows_b)}")
        compiled_a, compiled_b = sums(rows_a, "compiled_for_work"), sums(rows_b, "compiled_for_work")
        print(f"     a compiled {compiled_a} for the work, b {compiled_b}", flush=True)
        check(compiled_a > 0 and compiled_b > 0, "both workers compiled functions for the others")
        by_a = {row["unit"]: row for row in rows_a}
        shared = [
            row
            for row in rows_b
            if row["unit"] in by_a
            and by_a[row["unit"]]["execution"] == row["execution"]
            and row["source"] == "executed"
        ]
        check(bool(shared), "at least one unit was one execution of both daemons")
        for name, behaviour in (("a", run.behaviour("a")), ("b", run.behaviour("b"))):
            check(behaviour[0] == solo[0], f"worker {name}'s program behaves as solo's: {behaviour[0].strip()}")
            check(behaviour[1] == solo[1], f"worker {name}'s program is solo's, byte for byte")
        unexplained = [row for row in rows_a + rows_b if row["joined"] and row["tail_compiled"] > row["instances"]]
        check(not unexplained, "no unit compiled more than its functions")

        code, text = run.build("cold")
        check(code == 0, f"a third daemon builds from the cache: {text[-400:] if code else ''}")
        rows = run.report("cold")
        published = [row for row in rows if row["source"] == "published"]
        check(len(published) == len(rows) and rows, f"every unit of the cold daemon was published: {len(published)}/{len(rows)}")
        tail = sums(rows, "tail_compiled")
        check(tail == 0, f"the cold daemon compiled nothing: {tail}")
        behaviour = run.behaviour("cold")
        check(behaviour[0] == solo[0] and behaviour[1] == solo[1], "its program is solo's, byte for byte")

        before = len(run.report("cold"))
        code, _ = run.build("cold")
        check(code == 0 and len(run.report("cold")) == before, "a build with nothing changed runs no unit")

        # One function edited: what it reaches is worked again, by the same keys.
        edited = run.project / "mathy" / "src" / "lib.rs"
        edited.write_text(edited.read_text().replace('format!("3:{sum}', 'format!("3 edited:{sum}'))
        rows_before = len(run.report("a"))
        code, text = run.build("a")
        check(code == 0, f"an edit builds: {text[-400:] if code else ''}")
        new = run.report("a")[rows_before:]
        work = sums(new, "compiled_for_work") + sums(new, "tail_compiled")
        check(0 < work < sums(rows_a, "compiled_for_work") + sums(rows_b, "compiled_for_work"), f"the edit recompiled {work} functions, not the whole workspace")
        check(run.behaviour("a")[0] != solo[0], "the edited program behaves as edited")
        edited.write_text(edited.read_text().replace('format!("3 edited:{sum}', 'format!("3:{sum}'))

        if given.faults:
            # A worker killed in the middle: its lease expires and the other finishes the unit.
            shutil.rmtree(run.root / "blobs", ignore_errors=True)
            for name in ("lost", "survivor"):
                homes.append(name)
            edited.write_text(edited.read_text().replace('format!("7:{sum}', 'format!("7 lost:{sum}'))
            lost, _ = run.build("lost", wait=False)
            survivor, survivor_log = run.build("survivor", wait=False, BLAZE_DIST_DEADLINE_SECS="600")
            deadline = time.time() + 120
            while time.time() < deadline and not run.report("lost") and not run.report("survivor"):
                time.sleep(0.01)
            pid = daemon_pid(run, "lost")
            if pid:
                os.kill(pid, signal.SIGKILL)
            lost.wait()
            code = survivor.wait()
            check(code == 0, f"the survivor finished: {Path(survivor_log).read_text()[-800:] if code else ''}")
            behaviour = run.behaviour("survivor")
            check("functions" in behaviour[0], f"the survivor's program runs: {behaviour[0].strip()}")
            edited.write_text(edited.read_text().replace('format!("7 lost:{sum}', 'format!("7:{sum}'))

            # No carrier: a typed stop by default, the same compiler here when asked.
            run.coordinator.terminate()
            run.coordinator.wait()
            run.socket.unlink(missing_ok=True)
            edited.write_text(edited.read_text().replace('format!("9:{sum}', 'format!("9 down:{sum}'))
            code, text = run.build("a")
            check(code != 0 and "distributed build failed" in text, "no carrier stops the build, naming it")
            code, text = run.build("a", BLAZE_DIST_LOCAL="fallback")
            check(code == 0, f"fallback compiles here: {text[-400:] if code else ''}")
            check("functions" in run.behaviour("a")[0], "and the program runs")
        print("distv2: every check passed", flush=True)
    finally:
        run.stop(homes)


if __name__ == "__main__":
    if len(sys.argv) != 2:
        sys.exit(__doc__)
    try:
        main(load(sys.argv[1]))
    except Failed as failed:
        sys.exit(f"distv2: {failed}")
