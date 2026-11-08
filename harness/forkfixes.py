#!/usr/bin/env python3
"""Replay the fork's compiler fixes with Blaze and explicit semantic expectations.

One JSON object (inline, a file path, or `-`), with fields of Input below. `scratch` must be
new or empty: sources, full command/daemon logs, results.json and provenance.json stay there.
`source` names the kept compiler source tree that produced `blaze` (defaults to this checkout).
No reference compiler is run. A filtered run refuses unknown case names.

Every wait is bounded. A failed startup runs no cases; a case timeout stops the run so later
cases cannot overlap the daemon's cancelled work. `results.json` is checkpointed after each
case, including failures and cases not reached. `run.json` records setup/cleanup failures;
only a zero exit with every selected case ok is an acceptance result.
"""
from __future__ import annotations

import hashlib
import json
import os
import signal
import subprocess
import sys
import time
from dataclasses import asdict, dataclass
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE / "forkfixes"))
sys.path.insert(0, str(HERE.parent / "tools" / "typed-input" / "src"))
from cases import CASES  # noqa: E402
from storelib import Failed, artifact, blaze_status_of, messages, test_results, worked  # noqa: E402
from typed_input import parse  # noqa: E402


@dataclass(frozen=True)
class Input:
    blaze: str
    scratch: str
    only: tuple[str, ...] = ()
    stall_bound: int | None = None
    timeout: int = 1800
    source: str = str(HERE.parent)

    def __post_init__(self) -> None:
        for name in ("timeout", "stall_bound"):
            value = getattr(self, name)
            if value is not None and value <= 0:
                raise ValueError(f"{name} must be positive")
        unknown = set(self.only) - {case["name"] for case in CASES}
        if unknown:
            raise ValueError(f"unknown cases: {', '.join(sorted(unknown))}")


def save(path: Path, value: object) -> None:
    temporary = path.with_suffix(".tmp")
    temporary.write_text(json.dumps(value, indent=2) + "\n")
    temporary.replace(path)


def sha256(path: Path) -> str:
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def provenance(given: Input) -> dict:
    source = Path(given.source).resolve(strict=True)

    def git(*args: str) -> str:
        return subprocess.check_output(["git", "-C", str(source), *args], timeout=30).decode()

    names = git("ls-files", "--cached", "--others", "--exclude-standard", "-z").split("\0")
    files = {}
    for name in sorted(set(filter(None, names))):
        path = source / name
        files[name] = sha256(path) if path.is_file() else None
    encoded = json.dumps(files, sort_keys=True).encode()
    binaries = [Path(given.blaze).resolve(strict=True)]
    engine = binaries[0].with_name("blaze-engine")
    if engine.is_file():
        binaries.append(engine)
    # Include this lane's fixtures even when the compiler came from a separate frozen tree.
    harness = [Path(__file__).resolve(), *sorted((HERE / "forkfixes").glob("*.py"))]
    return {
        "source": str(source), "head": git("rev-parse", "HEAD").strip(),
        "git_tree": git("rev-parse", "HEAD^{tree}").strip(),
        "diff": git("diff", "--binary", "HEAD"),
        "status": git("status", "--porcelain"),
        "source_sha256": hashlib.sha256(encoded).hexdigest(), "files": files,
        "binaries": {str(path): sha256(path) for path in binaries},
        "harness": {str(path): sha256(path) for path in harness},
    }


def write(root: Path, files: dict[str, str]) -> None:
    for name, text in files.items():
        at = root / name
        at.parent.mkdir(parents=True, exist_ok=True)
        at.write_text(text)


def env_for(scratch: Path, home: str = "home") -> dict[str, str]:
    env = {k: v for k, v in os.environ.items() if not k.startswith(("CARGO", "RUST", "BLAZE_"))}
    # The installed library and the root's assigned resources are inputs, not harness defaults.
    for name in ("BLAZE_LIBRARY", "BLAZE_CORES", "BLAZE_DAEMON_MEMORY_GB"):
        if name in os.environ:
            env[name] = os.environ[name]
    env.update(BLAZE_HOME=str(scratch / home), TMPDIR=str(scratch / "tmp"), RUST_BACKTRACE="0")
    (scratch / "tmp").mkdir(parents=True, exist_ok=True)
    return env


def terminate(process: subprocess.Popen) -> None:
    """The session is ours, including children that outlived its leader; reap with a bound."""
    for sig in (signal.SIGTERM, signal.SIGKILL):
        try:
            os.killpg(process.pid, sig)
        except ProcessLookupError:
            pass
        if sig == signal.SIGTERM and process.poll() is not None:
            continue
        try:
            process.wait(timeout=5)
        except subprocess.TimeoutExpired:
            if sig == signal.SIGKILL:
                raise Failed(f"process group {process.pid} did not exit after SIGKILL")
        # Kill surviving descendants even if the leader exited on TERM.
    process.wait(timeout=5)


def command(argv: list[str], cwd: Path, env: dict, log: Path, timeout: float) -> dict:
    log.parent.mkdir(parents=True, exist_ok=True)
    result = {"argv": argv, "cwd": str(cwd), "timeout": timeout, "timed_out": False}
    started = time.monotonic()
    try:
        with log.with_suffix(".stdout").open("w") as out, log.with_suffix(".stderr").open("w") as err:
            process = subprocess.Popen(argv, cwd=cwd, env=env, stdin=subprocess.DEVNULL,
                                       stdout=out, stderr=err, start_new_session=True)
            try:
                process.wait(timeout=timeout)
            except subprocess.TimeoutExpired:
                result["timed_out"] = True
                terminate(process)
            except BaseException:
                terminate(process)
                raise
            result["returncode"] = process.returncode
    except OSError as error:
        result["error"] = str(error)
    finally:
        result["elapsed_seconds"] = time.monotonic() - started
        save(log.with_suffix(".json"), result)
    if result["timed_out"] or "error" in result:
        raise Failed(f"command failed: {result}; logs: {log}.*")
    return result


def said(log: Path) -> str:
    return "\n".join(log.with_suffix(suffix).read_text(errors="replace") for suffix in (".stdout", ".stderr"))


class Daemon:
    def __init__(self, given: Input, scratch: Path):
        self.given, self.scratch = given, scratch
        self.home = "home"
        self.process: subprocess.Popen | None = None
        self.generation = 0

    @property
    def env(self) -> dict:
        return env_for(self.scratch, self.home)

    def status(self, log: Path, timeout: float = 10) -> int | None:
        result = command([self.given.blaze, "status", "--json"], self.scratch, self.env, log, timeout)
        if result["returncode"] != 0:
            raise Failed(f"daemon status exited {result['returncode']}: {log}.*")
        status = blaze_status_of(log.with_suffix(".stdout").read_text())
        return status.daemon.pid if status.daemon else None

    def start(self) -> None:
        self.generation += 1
        log = self.scratch / "logs" / f"daemon-{self.generation}"
        argv = [self.given.blaze, "serve"]
        if self.given.stall_bound is not None:
            argv += ["--stall-bound", str(self.given.stall_bound)]
        with log.with_suffix(".log").open("w") as out:
            self.process = subprocess.Popen(argv, cwd=self.scratch, env=self.env,
                                            stdin=subprocess.DEVNULL, stdout=out,
                                            stderr=subprocess.STDOUT, start_new_session=True)
        save(log.with_suffix(".json"), {"argv": argv, "pid": self.process.pid, "home": self.env["BLAZE_HOME"]})
        deadline = time.monotonic() + 600
        attempt = 0
        while time.monotonic() < deadline:
            if self.process.poll() is not None:
                raise Failed(f"daemon exited {self.process.returncode} before readiness: {log}.log")
            attempt += 1
            pid = self.status(log.with_name(f"status-{self.generation}-{attempt}"), min(10, deadline - time.monotonic()))
            if pid == self.process.pid:
                return
            if pid is not None:
                raise Failed(f"daemon readiness found pid {pid}, expected {self.process.pid}: {log}.log")
            time.sleep(min(1, max(0, deadline - time.monotonic())))
        raise Failed(f"daemon did not become ready in 600 s: {log}.log")

    def check(self, log: Path) -> None:
        if self.process is None or self.process.poll() is not None:
            raise Failed(f"the owned daemon exited; refusing another build: {log}")
        if self.status(log) != self.process.pid:
            raise Failed(f"a different daemon served the case: {log}.*")

    def stop(self) -> None:
        if self.process is None:
            return
        try:
            result = command([self.given.blaze, "stop"], self.scratch, self.env,
                             self.scratch / "logs" / f"stop-{self.generation}", 60)
            if result["returncode"] != 0:
                raise Failed(f"blaze stop exited {result['returncode']}")
            try:
                self.process.wait(timeout=30)
            except subprocess.TimeoutExpired as error:
                raise Failed("daemon did not exit within 30 s after blaze stop") from error
        finally:
            terminate(self.process)
            self.process = None

    def replay(self) -> None:
        self.stop()
        # A new home has no daemon image or query state, only the first run's immutable store.
        store = Path(self.env["BLAZE_HOME"]) / "v2" / "store"
        self.home = "replay-home"
        target = Path(self.env["BLAZE_HOME"]) / "v2" / "store"
        target.parent.mkdir(parents=True)
        target.symlink_to(store, target_is_directory=True)
        self.start()


def run_case(daemon: Daemon, case: dict) -> tuple[bool, str]:
    scratch, given = daemon.scratch, daemon.given
    root = scratch / "cases" / case["name"]
    files = dict(case["files"])
    files.setdefault("Cargo.toml", f'[package]\nname = "{case["name"].replace("-", "_")}"\nversion = "0.1.0"\nedition = "2024"\n\n[workspace]\n')
    write(root, files)
    save(root / "sources.json", {name: sha256(root / name) for name in sorted(files)})
    logs = scratch / "logs" / case["name"]
    steps = case.get("steps", [{key: case[key] for key in ("prints", "error", "test") if key in case}])
    for index, step in enumerate(steps):
        if "files" in step:
            files.update(step["files"])
            write(root, step["files"])
        save(root / f"sources-{index}.json", {name: sha256(root / name) for name in sorted(files)})
        log = logs / f"{index}-build"
        daemon.check(log.with_name(f"{index}-before"))
        if step.get("replay"):
            daemon.replay()
        target = scratch / "targets" / case["name"] / ("replay" if step.get("replay") else "initial")
        argv = [given.blaze, "test" if "test" in step else "build", "--target-dir", str(target),
                "--message-format=json", *(["--reached"] if "test" in step else []), *step.get("args", ())]
        result = command(argv, root, daemon.env, log, given.timeout)
        daemon.check(log.with_name(f"{index}-after"))
        output = said(log)
        if "error" in step:
            if result["returncode"] <= 0 or step["error"] not in output:
                return False, f"step {index}: expected diagnostic {step['error']!r}, exit {result['returncode']}; {log}.*"
            continue
        if result["returncode"] != 0:
            return False, f"step {index}: build/test exited {result['returncode']}; {log}.*"
        if "test" in step:
            tests = [test for test in test_results(output) if test.name == step["test"]]
            if len(tests) != 1 or tests[0].outcome != "passed" or tests[0].cached is not None:
                return False, f"step {index}: test {step['test']!r} did not execute and pass once; {log}.*"
            continue
        for absent in step.get("absent", ()):
            if artifact(output, absent):
                return False, f"step {index}: built disabled program {absent}; {log}.*"
        work = worked(output)
        if step.get("replay") and (not any(item.found > 0 for item in work) or any(item.compiled for item in work)):
            return False, f"step {index}: store replay must report cached functions and compile none; {log}.*"
        program = step.get("program", case.get("program", case["name"].replace("-", "_")))
        executable = artifact(output, program)
        if not executable:
            return False, f"step {index}: build reported no executable for {program}; {log}.*"
        paths = {executable}
        for built in messages(output, "compiler-artifact"):
            paths.update(built.get("filenames", ()))
        save(log.with_name(f"{index}-artifacts.json"), {
            path: sha256(Path(path)) for path in sorted(paths) if Path(path).is_file()
        })
        ran_log = logs / f"{index}-run"
        ran = command([executable], root, daemon.env, ran_log, 120)
        got = ran_log.with_suffix(".stdout").read_text().splitlines()
        if ran["returncode"] != 0 or got != step["prints"]:
            return False, f"step {index}: exit {ran['returncode']}, printed {got!r}, expected {step['prints']!r}; {ran_log}.*"
    return True, f"{len(steps)} steps as stated; {logs}"


def main(given: Input) -> int:
    scratch = Path(given.scratch).resolve()
    # Never erase previous evidence or a daemon home belonging to another run.
    scratch.mkdir(parents=True, exist_ok=True)
    if any(scratch.iterdir()):
        raise ValueError(f"scratch must be new or empty, preserving existing evidence: {scratch}")
    (scratch / "logs").mkdir()
    given = Input(**{**asdict(given), "blaze": str(Path(given.blaze).resolve(strict=True))})
    selected = [case for case in CASES if not given.only or case["name"] in given.only]
    results = {case["name"]: {"ok": False, "why": "not run", "fixes": case["fixes"]} for case in selected}
    save(scratch / "results.json", results)
    report = {"input": asdict(given), "errors": [], "complete": False}
    daemon = Daemon(given, scratch)
    before = None
    try:
        before = provenance(given)
        save(scratch / "provenance.json", before)
        save(scratch / "environment.json", {name: daemon.env.get(name) for name in (
            "BLAZE_HOME", "BLAZE_LIBRARY", "BLAZE_CORES", "BLAZE_DAEMON_MEMORY_GB",
            "LLVM_SYS_231_PREFIX", "XDG_CACHE_HOME", "TMPDIR", "RUST_BACKTRACE")})
        daemon.start()
        for case in selected:
            try:
                ok, why = run_case(daemon, case)
            except (Failed, OSError, ValueError, subprocess.SubprocessError, KeyboardInterrupt) as error:
                results[case["name"]].update(ok=False, why=f"{type(error).__name__}: {error}")
                save(scratch / "results.json", results)
                raise
            results[case["name"]].update(ok=ok, why=why)
            save(scratch / "results.json", results)
            print(("ok   " if ok else "FAIL ") + f"{case['name']}: {why}", flush=True)
        report["complete"] = True
    except (Failed, OSError, ValueError, subprocess.SubprocessError, KeyboardInterrupt) as error:
        report["errors"].append(f"{type(error).__name__}: {error}")
    finally:
        try:
            daemon.stop()
        except (Failed, OSError, subprocess.SubprocessError) as error:
            report["errors"].append(f"cleanup: {error}")
        if before is not None:
            try:
                after = provenance(given)
                save(scratch / "provenance-after.json", after)
                if after != before:
                    report["errors"].append("source, harness or binary changed during acceptance; see provenance-after.json")
            except (OSError, subprocess.SubprocessError) as error:
                report["errors"].append(f"final provenance: {error}")
        save(scratch / "run.json", report)
        save(scratch / "results.json", results)
    passed = sum(it["ok"] for it in results.values())
    print(f"forkfixes: {passed} of {len(results)} as stated; results: {scratch}", flush=True)
    for error in report["errors"]:
        print(f"FAIL {error}", flush=True)
    return int(bool(report["errors"]) or passed != len(results) or not report["complete"])


if __name__ == "__main__":
    if len(sys.argv) != 2:
        sys.exit(__doc__)
    try:
        sys.exit(main(parse(Input, sys.argv[1], "forkfixes.py")))
    except (OSError, ValueError) as error:
        sys.exit(f"forkfixes.py: {error}")
