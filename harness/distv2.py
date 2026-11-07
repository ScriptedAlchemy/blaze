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
    faults       (optional) run the fault cases; default true
    profile      (optional) the directory under the target directory programs are in; default "debug"
    stall_bound  (optional) seconds a daemon's loop may make no progress (`blaze serve --stall-bound`):
                 an unoptimized blaze under test needs more than the default
    keep_solo    (optional) keep the solo daemon's home from a run before, which is what an
                 unoptimized blaze takes longest over; default false

Checks, each on what the build itself reports (BLAZE_DIST_REPORT) and on running the program:
    solo         one daemon, no coordinator: the program the others are held to
    two workers  two daemons build at once: every unit done once by agreement, both compile, both
                 programs behave as solo's, and neither compiles anything nothing committed
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
    keep_solo: bool = False


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
        if given.keep_solo and self.root.exists():
            for it in self.root.iterdir():
                if it.name not in ("solo", "project", "targets"):
                    shutil.rmtree(it, ignore_errors=True) if it.is_dir() else it.unlink()
            for it in (self.root / "targets").iterdir():
                if it.name != "solo":
                    shutil.rmtree(it, ignore_errors=True)
        else:
            shutil.rmtree(self.root, ignore_errors=True)
        (self.root / "logs").mkdir(parents=True)
        self.socket = Path("/tmp") / f"blzd-{os.getpid()}.sock"
        self.port = free_port()
        self.daemons: list[Path] = []
        self.services: list[subprocess.Popen] = []
        self.lease: list[str] = []
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
            [self.given.dist_local, "coordinate", str(self.socket), triple, "64", *self.lease],
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

    def daemon(self, home: str) -> None:
        """Starts `home`'s daemon with the given stall bound; a client would start it with the default."""
        if self.given.stall_bound is None:
            return
        at = self.root / home / "v2.sock"
        if at.exists() and daemon_pid(self, home):
            return
        log = open(self.root / "logs" / f"{home}.daemon.log", "a")
        subprocess.Popen(
            [self.given.blaze, "serve", "--stall-bound", str(self.given.stall_bound)],
            cwd=self.project,
            env=self.env(home, False),
            stdout=log,
            stderr=subprocess.STDOUT,
            start_new_session=True,
        )
        deadline = time.time() + 300
        while time.time() < deadline and not daemon_pid(self, home):
            time.sleep(0.2)

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
        )
        if home not in self.daemons:
            self.daemons.append(home)
        if not wait:
            return process, log.name
        code = process.wait()
        return code, Path(log.name).read_text()

    def target(self, home: str, project: Path | None = None) -> Path:
        """Each daemon's target directory of a checkout, as a person on that machine would have."""
        return self.root / "targets" / home / (project or self.project).name

    def program(self, home: str, project: Path | None = None) -> Path:
        return self.target(home, project) / self.given.profile / self.given.program

    def behaviour(self, home: str, project: Path | None = None) -> tuple[str, str]:
        program = self.program(home, project)
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
        return (json.loads(out.stdout).get("daemon") or {}).get("pid")
    except json.JSONDecodeError:
        return None


def sums(rows: list[dict], name: str) -> int:
    return sum(row[name] for row in rows)


def edit(path: Path, old: str, new: str) -> None:
    text = path.read_text()
    if old not in text:
        raise Failed(f"{path} has no {old!r}")
    path.write_text(text.replace(old, new))


def main(given: Input) -> None:
    run = Run(given)
    run.project = run.root / "project"
    shutil.copytree(given.project, run.project, ignore=shutil.ignore_patterns("target", ".git"), dirs_exist_ok=True)
    homes = ["solo", "a", "b", "cold"]
    edited = run.project / "mathy" / "src" / "lib.rs"
    try:
        run.start_services()
        code, text = run.build("solo", dist=False)
        check(code == 0, f"solo builds: {text[-400:] if code else ''}")
        solo = run.behaviour("solo")
        print("     solo:", solo[0].strip().replace("\n", " | "), solo[1][:16], flush=True)
        check("caught true" in solo[0] and "script says from the script" in solo[0] and "seen by the macro" in solo[0], "the program unwinds across units and has its script's and its proc macro's output")

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
        print(f"     one execution of both: {sorted(row['unit'] for row in shared)}", flush=True)
        check(bool(shared), "at least one unit was one execution of both daemons")
        for name, behaviour in (("a", run.behaviour("a")), ("b", run.behaviour("b"))):
            check(behaviour[0] == solo[0], f"worker {name}'s program behaves as solo's")
            check(behaviour[1] == solo[1], f"worker {name}'s program is solo's, byte for byte")
        unexplained = [row for row in rows_a + rows_b if row["joined"] and row["tail_compiled"] > row["instances"]]
        check(not unexplained, "no unit compiled more than its functions")

        code, text = run.build("cold")
        check(code == 0, f"a third daemon builds from the cache: {text[-400:] if code else ''}")
        rows = run.report("cold")
        published = [row for row in rows if row["source"] == "published"]
        print(f"     cold: {len(published)}/{len(rows)} published; compiled {sums(rows, 'tail_compiled')} of {sums(rows, 'instances')}", flush=True)
        check(len(published) == len(rows) and rows, f"every unit of the cold daemon was published: {len(published)}/{len(rows)}")
        tail = sums(rows, "tail_compiled")
        check(tail == 0, f"the cold daemon compiled nothing: {tail}")
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
        check(run.behaviour("a")[0] != solo[0], "the edited program behaves as edited")
        edit(edited, 'format!("3 edited:{sum}', 'format!("3:{sum}')

        # A file a unit includes is an input of its keys, and so a new unit: nothing stale is taken.
        included = run.project / "shapes" / "included.txt"
        keep = included.read_text()
        included.write_text("changed by the edit")
        code, text = run.build("cold")
        check(code == 0, f"an edit of an included file builds: {text[-400:] if code else ''}")
        check("included changed by the edit" in run.behaviour("cold")[0], "and the program has the new text, not a published one's")
        included.write_text(keep)

        # Another pull request's checkout, somewhere else, with one crate edited: what it shares
        # by content comes from the cache, what it does not is compiled and published.
        other = run.root / "project-pr"
        shutil.copytree(run.project, other, ignore=shutil.ignore_patterns("target", ".git"))
        homes.append("pr")
        edit(other / "mathy" / "src" / "lib.rs", 'format!("5:{sum}', 'format!("5 pr:{sum}')
        code, text = run.build("pr", project=other, BLAZE_DIST_PR="17", BLAZE_DIST_SOURCE="rev1")
        check(code == 0, f"another pull request builds: {text[-400:] if code else ''}")
        rows = run.report("pr")
        taken = [row for row in rows if row["source"] == "published"]
        print(f"     pr: {len(taken)}/{len(rows)} units published already: {sorted(row['unit'] for row in taken)}", flush=True)
        check(any(row["unit"] == "shapes" for row in taken), "the crate it shares came from the cache")
        check(sums(taken, "tail_compiled") == 0 and sums(taken, "compiled_for_work") == 0, "and compiled nothing here")
        check(any(row["unit"] == "mathy" and row["source"] == "executed" for row in rows), "the edited crate was built")
        check(run.behaviour("pr", other)[0] != solo[0], "its program is the edited one")
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
        code_newer, code_older = newer.wait(), older.wait()
        check(code_newer == 0, f"the newer revision builds: {Path(newer_log).read_text()[-800:] if code_newer else ''}")
        print(f"     the older revision's build: exit {code_older}", flush=True)
        check("newer" not in run.behaviour("a", older_at)[0] if code_older == 0 else "distributed build failed" in Path(older_log).read_text(), "the older one finished as itself, or stopped naming the distributed path")

        if given.faults:
            # A worker killed in the middle: its lease expires and the other finishes the unit.
            edit(edited, 'format!("7:{sum}', 'format!("7 lost:{sum}')
            mark = len(run.report("b")), len(run.report("a"))
            lost, _ = run.build("a", wait=False)
            survivor, survivor_log = run.build("b", wait=False, BLAZE_DIST_DEADLINE_SECS="1800")
            deadline = time.time() + 1800
            while time.time() < deadline and len(run.report("a")) == mark[1] and len(run.report("b")) == mark[0] and survivor.poll() is None:
                time.sleep(0.01)
            pid = daemon_pid(run, "a")
            check(pid is not None, "the worker to kill has a daemon")
            os.kill(pid, signal.SIGKILL)
            lost.wait()
            code = survivor.wait()
            check(code == 0, f"the survivor finished: {Path(survivor_log).read_text()[-800:] if code else ''}")
            code, text = run.build("solo", dist=False)
            check(code == 0, f"the control builds alone: {text[-400:] if code else ''}")
            control = run.behaviour("solo")
            check(run.behaviour("b") == control, "the survivor's program is the one built alone, byte for byte")
            edit(edited, 'format!("7 lost:{sum}', 'format!("7:{sum}')

            # A worker stopped past its lease and then resumed: its lease is taken back, whatever
            # it reports afterwards is refused, and both builds end with the same program.
            run.coordinator.terminate()
            run.coordinator.wait()
            run.lease = ["2000"]
            run.start_coordinator()
            edit(edited, 'format!("8:{sum}', 'format!("8 fence:{sum}')
            code, text = run.build("solo", dist=False)
            check(code == 0, f"the fence's control builds: {text[-400:] if code else ''}")
            control = run.behaviour("solo")
            fenced_log = run.root / "logs" / "coordinator.log"
            before_fence = len(fenced_log.read_text().splitlines())
            packs = run.root / "blobs" / "packs"
            before_packs = set(packs.glob("*.pack"))
            stalled, stalled_log = run.build("a", wait=False, BLAZE_DIST_DEADLINE_SECS="1800")
            healthy, healthy_log = run.build("b", wait=False, BLAZE_DIST_DEADLINE_SECS="1800")
            deadline = time.time() + 1800
            stopped = None
            while time.time() < deadline and stalled.poll() is None and stopped is None:
                # As a new batch is committed, the work is under leases of both.
                if set(packs.glob("*.pack")) - before_packs:
                    stopped = daemon_pid(run, "a")
                    if stopped:
                        os.kill(stopped, signal.SIGSTOP)
                time.sleep(0.02)
            check(stopped is not None, "a worker was stopped in the middle of its build")
            time.sleep(8)
            os.kill(stopped, signal.SIGCONT)
            code_h, code_s = healthy.wait(), stalled.wait()
            check(code_h == 0, f"the healthy worker finished: {Path(healthy_log).read_text()[-800:] if code_h else ''}")
            check(code_s == 0, f"the resumed worker finished: {Path(stalled_log).read_text()[-800:] if code_s else ''}")
            lines = fenced_log.read_text().splitlines()[before_fence:]
            print(f"     coordinator: {len(lines)} lines, e.g. {lines[:3]}", flush=True)
            check(any(("fenced" in it or "holder-lost" in it or "lead-lost" in it) for it in lines), "the coordinator took the stopped worker's work back")
            for name in ("a", "b"):
                check(run.behaviour(name) == control, f"{name}'s program is the one built alone, byte for byte")
            edit(edited, 'format!("8 fence:{sum}', 'format!("8:{sum}')

            # A pack damaged on the cache server: the read refuses it, and the unit is not taken.
            edit(edited, 'format!("2:{sum}', 'format!("2 damaged:{sum}')
            before_packs = set(packs.glob("*.pack"))
            code, text = run.build("a")
            check(code == 0, f"a worker publishes what the next two take: {text[-400:] if code else ''}")
            code, text = run.build("solo", dist=False)
            check(code == 0, f"the control builds alone: {text[-400:] if code else ''}")
            control = run.behaviour("solo")
            for target in sorted(set(packs.glob("*.pack")) - before_packs):
                raw = bytearray(target.read_bytes())
                raw[len(raw) // 2] ^= 0xff
                target.write_bytes(bytes(raw))
            code, text = run.build("b", BLAZE_DIST_LOCAL="never")
            check(code != 0 and "distributed build failed" in text, "under `never` a damaged pack stops the build, naming it")
            code, text = run.build("cold", BLAZE_DIST_LOCAL="fallback")
            check(code == 0, f"under `fallback` it compiles here: {text[-400:] if code else ''}")
            check(run.behaviour("cold") == control, "and its program is the one built alone, byte for byte")
            edit(edited, 'format!("2 damaged:{sum}', 'format!("2:{sum}')

            # No carrier: a typed stop by default, the same compiler here when asked.
            run.coordinator.terminate()
            run.coordinator.wait()
            run.socket.unlink(missing_ok=True)
            edit(edited, 'format!("9:{sum}', 'format!("9 down:{sum}')
            code, text = run.build("a")
            check(code != 0 and "distributed build failed" in text, "no carrier stops the build, naming it")
            code, text = run.build("a", BLAZE_DIST_LOCAL="fallback")
            check(code == 0, f"fallback compiles here: {text[-400:] if code else ''}")
            check("functions" in run.behaviour("a")[0] and "caught true" in run.behaviour("a")[0], "and the program runs")
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
