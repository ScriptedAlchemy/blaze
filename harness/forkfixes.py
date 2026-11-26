#!/usr/bin/env python3
"""Run every retained fork-fix fixture through the maintained blaze interfaces.

One JSON object, inline, a file, or `-`:

    blaze    the source `blaze` executable (its launcher and fact shim, when beside it,
             are hashed before and after the run too)
    scratch  a new or empty directory that receives all projects, homes and evidence
    shelf    the canonical `.blaze/v2/toolchain` shelf copied into each isolated HOME;
             omitted: `$HOME/.blaze/v2/toolchain` of the harness process
    only     case names to run (empty: all 23)
    timeout  seconds bounding each source command (1800 by default)
    source   the compiler source checkout (defaults to this checkout)
    workers  worker count passed explicitly to check/run/test/export (optional)
    stall_bound  accepted for compatibility with the legacy harness input; the v2
             checkout daemon has no such tuning and the value is reported as ignored

Each case gets a fresh project, HOME and explicitly started checkout-scoped daemon.
Diagnostics come from `blaze check --view json`; execution from `blaze run`; tests from
`blaze test -- --format json --run-all --exact`; the executable/cdylib artifact and
cached-replay oracle from `blaze export --stats` (a cdylib through explicit `--lib`) and
exported programs. Required-feature absence is read from `blaze check --view json`'s
target-not-built notes. No legacy
`build`, `serve`, `status --json`, `--message-format` or `blaze-test-result` interface
is used. Independent semantic failures are collected. A timeout or an ownership failure stops the run.
"""
from __future__ import annotations

from dataclasses import dataclass
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import signal
import subprocess
import sys
import tempfile
import time
import tomllib

sys.path.insert(0, str(Path(__file__).resolve().parent / "forkfixes"))
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "tools/typed-input/src"))
from cases import CASES  # noqa: E402
from typed_input import parse  # noqa: E402

RESOURCE_ENV = ("BLAZE_LIBRARY", "BLAZE_CORES", "BLAZE_DAEMON_MEMORY_GB", "BLAZE_HUGE_PAGES",
                "BLAZE_TESTS", "BLAZE_STORE_MAX_BYTES", "BLAZE_STORE_MAX_GB")


class Failed(ValueError):
    """The harness or its input is wrong."""


class CaseFailed(ValueError):
    """One fixture's oracle did not hold; independent fixtures still run."""


@dataclass(frozen=True)
class Input:
    blaze: str
    scratch: str
    shelf: str = ""
    only: tuple[str, ...] = ()
    timeout: int = 1800
    source: str | None = None
    workers: int | None = None
    stall_bound: int | None = None

    def __post_init__(self) -> None:
        for name in ("timeout", "workers", "stall_bound"):
            value = getattr(self, name)
            if value is not None and (type(value) is not int or value <= 0):
                raise Failed(f"{name} must be a positive integer")
        unknown = sorted(set(self.only) - {case["name"] for case in CASES})
        if unknown:
            raise Failed(f"unknown case names in only: {', '.join(unknown)}")


def digest(path: Path) -> str:
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def require(condition: bool, message: str) -> None:
    if not condition:
        raise CaseFailed(message)


def validate(given: Input, harness_home: str | None) -> Input:
    blaze = Path(given.blaze)
    if blaze.is_symlink() or not blaze.is_file() or not os.access(blaze, os.X_OK):
        raise Failed(f"blaze {blaze} is not a regular executable")
    scratch = Path(given.scratch)
    if scratch.is_symlink() or (scratch.exists() and (not scratch.is_dir() or any(scratch.iterdir()))):
        raise Failed(f"scratch {scratch} must be new or empty; earlier evidence is never erased")
    shelf = given.shelf
    if not shelf:
        if not harness_home:
            raise Failed("shelf was omitted and the harness process has no HOME")
        shelf = str(Path(harness_home) / ".blaze/v2/toolchain")
    shelf_path = Path(shelf)
    if not shelf_path.is_dir() or not any(shelf_path.iterdir()):
        raise Failed(f"shelf {shelf_path} is not a nonempty toolchain shelf")
    if any(path.is_symlink() for path in shelf_path.rglob("*")):
        raise Failed(f"shelf {shelf_path} contains a symlink; copy the canonical shelf")
    return Input(str(blaze.absolute()), str(scratch.absolute()), str(shelf_path.resolve()),
                 given.only, given.timeout, given.source, given.workers, given.stall_bound)


def binary_digests(blaze: Path) -> dict[str, str]:
    result = {"blaze": digest(blaze)}
    for name in ("blaze-launcher", "blaze-fact-shim", "blaze-engine"):
        path = blaze.with_name(name)
        if path.is_file():
            result[name] = digest(path)
    return result


def provenance(given: Input) -> dict:
    root = Path(__file__).resolve().parents[1]
    source = Path(given.source or root).resolve(strict=True)

    def git(*args: str) -> bytes:
        return subprocess.check_output(["git", "-C", str(source), *args], timeout=30)

    # HEAD and the tracked patch do not cover untracked Rust files. Scratch was empty at
    # entry and owns its outputs, even when it is beneath the source checkout.
    untracked = {}
    for name in git("ls-files", "--others", "--exclude-standard", "-z").split(b"\0"):
        if name:
            path = source / os.fsdecode(name)
            if not path.is_relative_to(Path(given.scratch).resolve()):
                untracked[os.fsdecode(name)] = digest(path) if path.is_file() else os.readlink(path)
    harness = [Path(__file__).resolve(), root / "harness/forkfixes/cases.py",
               root / ".github/scripts/forkfixes_acceptance.py",
               *sorted((root / "tools/typed-input/src").rglob("*.py"))]
    return {"source": str(source), "source_head": git("rev-parse", "HEAD").decode().strip(),
            "git_tree": git("rev-parse", "HEAD^{tree}").decode().strip(),
            "patch_sha256": hashlib.sha256(git("diff", "--no-ext-diff", "--binary", "HEAD")).hexdigest(),
            "untracked_sha256": tree_identity(untracked), "binaries": binary_digests(Path(given.blaze)),
            "harness": {str(path.relative_to(root)): digest(path) for path in harness},
            "resources": {name: os.environ[name] for name in RESOURCE_ENV if name in os.environ},
            "workers": given.workers, "shelf": shelf_provenance(Path(given.shelf))}


def shelf_provenance(shelf: Path) -> dict:
    files = sorted(path for path in shelf.iterdir() if path.is_file())
    answer = {"path": str(shelf), "files": len(files)}
    manifests = []
    for path in files:
        if re.fullmatch(r"[0-9a-f]{64}", path.name):
            with path.open("rb") as stream:
                if stream.read(8) == b"BLZTCH03":
                    manifests.append({"name": path.name, "sha256": digest(path)})
    if manifests:
        answer["toolchain_manifests"] = manifests
    return answer


def environment(home: Path) -> dict[str, str]:
    env = {key: value for key, value in os.environ.items()
           if not key.startswith(("CARGO", "RUST", "BLAZE_"))}
    env.update({
        "HOME": str(home),
        "TMPDIR": str(home / "tmp"),
        "RUST_BACKTRACE": "0",
        "BLAZE_LOCK": "write",
        "PYTHONDONTWRITEBYTECODE": "1",
    })
    for key in RESOURCE_ENV:
        if key in os.environ:
            env[key] = os.environ[key]
    return env


def logical_home(target: Path) -> tuple[Path, Path]:
    """A short logical HOME path for Unix-socket use, backed by evidence kept in scratch."""
    container = Path(tempfile.mkdtemp(prefix="blaze-ff-", dir="/tmp"))
    link = container / "h"
    link.symlink_to(target, target_is_directory=True)
    return link, container


def cleanup_logical_home(container: Path) -> None:
    link = container / "h"
    if link.is_symlink() or link.exists():
        link.unlink()
    container.rmdir()


def copy_shelf(shelf: Path, home: Path) -> None:
    """Place the immutable shelf in a fresh HOME, hard-linking its read-only packs when possible."""
    destination = home / ".blaze/v2/toolchain"
    destination.mkdir(parents=True)
    for source in sorted(shelf.rglob("*")):
        target = destination / source.relative_to(shelf)
        if source.is_dir():
            target.mkdir()
        elif source.is_file():
            try:
                os.link(source, target)
            except OSError:
                shutil.copy2(source, target)
    (home / "tmp").mkdir()


def write_files(root: Path, files: dict[str, str]) -> dict[str, str]:
    written = {}
    for name, content in files.items():
        path = root / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(content)
        written[name] = hashlib.sha256(content.encode()).hexdigest()
    return written


def initial_files(case: dict) -> dict[str, str]:
    files = dict(case["files"])
    if "Cargo.toml" not in files:
        package = case["name"].replace("-", "_")
        files["Cargo.toml"] = (f'[package]\nname = "{package}"\nversion = "0.1.0"\n'
                               'edition = "2024"\n\n[workspace]\n')
    return files


def steps_for(case: dict) -> list[dict]:
    return case.get("steps", [{key: case[key] for key in ("prints", "diagnostic", "error", "test") if key in case}])


def manifests(root: Path) -> dict[str, dict]:
    """Package and binary target facts read from the fixture manifests."""
    found: dict[str, dict] = {}

    def one(manifest_path: Path) -> None:
        try:
            manifest = tomllib.loads(manifest_path.read_text())
        except (OSError, tomllib.TOMLDecodeError):
            return
        package = manifest.get("package")
        if not isinstance(package, dict) or not isinstance(package.get("name"), str):
            return
        directory = manifest_path.parent
        bins: dict[str, dict] = {}
        if (directory / "src/main.rs").is_file():
            bins[package["name"]] = {"package": package["name"], "required_features": []}
        for target in manifest.get("bin", []):
            if isinstance(target, dict) and isinstance(target.get("name"), str):
                required = target.get("required-features", [])
                bins[target["name"]] = {"package": package["name"],
                                        "required_features": required if isinstance(required, list) else []}
        lib = manifest.get("lib", {})
        crate_types = lib.get("crate-type", []) if isinstance(lib, dict) else []
        found[package["name"]] = {"manifest": manifest_path, "bins": bins,
                                  "cdylib": "cdylib" in crate_types}

    root_manifest = root / "Cargo.toml"
    one(root_manifest)
    try:
        workspace = tomllib.loads(root_manifest.read_text()).get("workspace", {})
    except (OSError, tomllib.TOMLDecodeError):
        workspace = {}
    for member in workspace.get("members", []) if isinstance(workspace, dict) else []:
        if isinstance(member, str):
            for directory in sorted(root.glob(member)):
                if (directory / "Cargo.toml").is_file():
                    one(directory / "Cargo.toml")
    return found


def choose_target(root: Path, case: dict, step: dict) -> tuple[str, str]:
    """The package and binary the original target-selection oracle names."""
    args = list(step.get("args", []))
    requested = step.get("program", case.get("program"))
    for index, arg in enumerate(args[:-1]):
        if arg == "--bin":
            requested = args[index + 1]
    packages = manifests(root)
    binaries = [(package, name) for package, fact in packages.items() for name in fact["bins"]]
    if requested is None:
        if len(binaries) != 1:
            raise CaseFailed(f"fixture {case['name']} has no program key and {len(binaries)} binary targets")
        return binaries[0]
    exact = [(package, name) for package, name in binaries if name == requested]
    if len(exact) != 1:
        raise CaseFailed(f"fixture {case['name']} has {len(exact)} targets named {requested!r}, expected one")
    return exact[0]


def target_args(root: Path, case: dict, step: dict) -> list[str]:
    package, binary = choose_target(root, case, step)
    args = ["-p", package, "--bin", binary]
    raw = list(step.get("args", []))
    index = 0
    while index < len(raw):
        if raw[index] == "--bin" and index + 1 < len(raw):
            index += 2  # target selection is represented exactly by -p/--bin above
        else:
            args.append(raw[index])
            index += 1
    return args


def worker_args(given: Input) -> list[str]:
    return ["--workers", str(given.workers)] if given.workers is not None else []


def save(path: Path, value: object) -> None:
    temporary = path.with_suffix(".tmp")
    temporary.write_text(json.dumps(value, indent=2) + "\n")
    temporary.replace(path)


def terminate(process: subprocess.Popen) -> None:
    """Signal the entire owned session, including descendants after the leader exits."""
    for sig in (signal.SIGTERM, signal.SIGKILL):
        try:
            os.killpg(process.pid, sig)
        except ProcessLookupError:
            pass
        try:
            process.wait(timeout=5)
        except subprocess.TimeoutExpired as error:
            if sig == signal.SIGKILL:
                raise Failed(f"owned process group {process.pid} did not exit after SIGKILL") from error


def run_command(argv: list[str], cwd: Path, env: dict[str, str], timeout: int,
                evidence: Path, label: str) -> dict:
    """Keep full output on disk, including when a process is killed or cannot be reaped."""
    evidence.parent.mkdir(parents=True, exist_ok=True)
    started = time.monotonic()
    record = {"label": label, "argv": [str(part) for part in argv], "cwd": str(cwd),
              "timeout": timeout, "returncode": None, "timed_out": False}
    stdout, stderr = evidence.with_suffix(".stdout"), evidence.with_suffix(".stderr")
    try:
        with stdout.open("wb") as out, stderr.open("wb") as err:
            child = subprocess.Popen(record["argv"], cwd=cwd, env=env, stdin=subprocess.DEVNULL,
                                     stdout=out, stderr=err, start_new_session=True)
            try:
                child.wait(timeout=timeout)
            except subprocess.TimeoutExpired:
                record["timed_out"] = True
                terminate(child)
            except BaseException:
                terminate(child)
                raise
            record["returncode"] = child.returncode
    finally:
        record["elapsed_seconds"] = time.monotonic() - started
        record["stdout"] = stdout.read_text(errors="replace") if stdout.exists() else ""
        record["stderr"] = stderr.read_text(errors="replace") if stderr.exists() else ""
        save(evidence, record)
    return record


class CommandLog:
    def __init__(self, root: Path, case: str):
        self.directory = root / "evidence" / case
        self.count = 0

    def __call__(self, argv: list[str], cwd: Path, env: dict[str, str], timeout: int,
                 label: str) -> dict:
        self.count += 1
        safe = re.sub(r"[^A-Za-z0-9_.-]+", "-", label)
        evidence = self.directory / f"{self.count:03d}-{safe}.json"
        record = run_command(argv, cwd, env, timeout, evidence, label)
        if record["timed_out"]:
            raise Failed(f"{label} timed out after {timeout}s; see {evidence}")
        return record


def daemon_status_pid(stdout: str) -> int | None:
    try:
        said = json.loads(stdout)
    except json.JSONDecodeError:
        return None
    daemon = said.get("daemon") if isinstance(said, dict) else None
    return daemon.get("pid") if isinstance(daemon, dict) and type(daemon.get("pid")) is int else None


class CheckoutDaemon:
    """An explicitly owned foreground `blaze daemon` for one checkout and HOME."""

    def __init__(self, blaze: Path, root: Path, home: Path, env: dict[str, str], log: CommandLog):
        self.blaze = blaze
        self.root = root
        self.home = home
        self.env = env
        self.log = log
        self.process: subprocess.Popen | None = None
        self.output = log.directory / "daemon.log"
        self.output.parent.mkdir(parents=True, exist_ok=True)

    def status(self, timeout: float = 10) -> int | None:
        record = self.log([self.blaze, "daemon", "--status"], self.root, self.env, timeout, "daemon-status")
        if record["returncode"] != 0:
            return None
        return daemon_status_pid(record["stdout"])

    def start(self) -> None:
        with self.output.open("ab") as stream:
            self.process = subprocess.Popen([str(self.blaze), "daemon"], cwd=self.root,
                                            env=self.env, stdin=subprocess.DEVNULL,
                                            stdout=stream, stderr=subprocess.STDOUT, start_new_session=True)
        deadline = time.monotonic() + 30
        while (remaining := deadline - time.monotonic()) > 0:
            if self.process.poll() is not None:
                raise Failed(f"checkout daemon for {self.root} exited {self.process.returncode} during startup")
            pid = self.status(min(10, remaining))
            if pid == self.process.pid:
                return
            if pid is not None:
                raise Failed(f"checkout daemon for {self.root} reports pid {pid}, expected {self.process.pid}")
            time.sleep(min(0.25, remaining))
        raise Failed(f"checkout daemon for {self.root} did not report its pid within 30 s")

    def check_alive(self) -> None:
        if self.process is None or self.process.poll() is not None:
            raise Failed(f"owned checkout daemon for {self.root} exited during the case")
        if self.status() != self.process.pid:
            raise Failed(f"checkout daemon status for {self.root} did not report owned pid {self.process.pid}")

    def stop(self) -> None:
        process = self.process
        if process is None:
            return
        try:
            if process.poll() is not None:
                raise Failed(f"owned daemon exited {process.returncode} before daemon --stop")
            # A stale or foreign socket must never receive our stop request.
            self.check_alive()
            record = self.log([self.blaze, "daemon", "--stop"], self.root, self.env, 20, "daemon-stop")
            if record["returncode"] != 0:
                raise Failed(f"daemon --stop returned {record['returncode']}")
            try:
                status = process.wait(timeout=20)
            except subprocess.TimeoutExpired as error:
                raise Failed("owned daemon did not exit after daemon --stop") from error
            if status != 0:
                raise Failed(f"owned daemon exited {status} despite daemon --stop")
        finally:
            try:
                terminate(process)
            finally:
                self.process = None


def check_rows(record: dict) -> tuple[list[dict], dict]:
    rows = []
    try:
        rows = [json.loads(line) for line in record["stdout"].splitlines() if line.strip()]
    except json.JSONDecodeError as error:
        raise CaseFailed(f"check --view json did not produce only JSON rows: {error}") from error
    summaries = [row for row in rows if isinstance(row, dict) and "errors" in row]
    if len(summaries) != 1 or rows[-1] is not summaries[0]:
        raise CaseFailed("check --view json has no exactly-one closing summary")
    return rows[:-1], summaries[0]


def diagnostic_matches(rows: list[dict], expected: str) -> bool:
    for row in rows:
        if not isinstance(row, dict) or row.get("severity") != "error":
            continue
        message = row.get("message") if isinstance(row.get("message"), str) else ""
        code = row.get("code") if isinstance(row.get("code"), str) else ""
        if re.fullmatch(r"E\d+", expected):
            if code == expected or (code == "types" and re.search(r"\(" + expected + r" [A-Za-z]+\)$", message)):
                return True
        elif expected in message or expected in code:
            return True
    return False


def binary_package(root: Path, name: str) -> str:
    matches = sorted(package for package, fact in manifests(root).items() if name in fact["bins"])
    if len(matches) != 1:
        raise CaseFailed(f"binary target {name!r} is named by {len(matches)} packages, not exactly one")
    return matches[0]


def skipped_target_note(rows: list[dict], name: str, package: str) -> bool:
    prefix = f"bin `{name}` of package `{package}` is not built:"
    return any(isinstance(row, dict) and isinstance(row.get("note"), str)
               and row["note"].startswith(prefix) for row in rows)


def perform_absent(given: Input, root: Path, env: dict[str, str], log: CommandLog,
                   name: str) -> None:
    package = binary_package(root, name)
    record = log([given.blaze, "check", "--view", "json", *worker_args(given)], root, env,
                 given.timeout, f"check-absent-{name}")
    require(not record["timed_out"], f"check for absent target {name} timed out")
    require(record["returncode"] == 0,
            f"workspace check could not prove {name!r} absent: exit {record['returncode']}: "
            f"{record['stderr'][-500:]}")
    rows, summary = check_rows(record)
    require(summary.get("errors") == 0,
            f"workspace check could not prove {name!r} absent with errors={summary.get('errors')!r}")
    require(summary.get("not_checked") == [],
            f"workspace check was incomplete while proving {name!r} absent: "
            f"not_checked={summary.get('not_checked')!r}")
    require(skipped_target_note(rows, name, package),
            f"check JSON has no maintained note that bin {name!r} of package {package!r} is not built")


def perform_diagnostic(given: Input, root: Path, env: dict[str, str], log: CommandLog,
                       expected: str) -> None:
    record = log([given.blaze, "check", "--view", "json", *worker_args(given)], root, env,
                 given.timeout, f"check-{expected}")
    require(not record["timed_out"], "check timed out")
    require(record["returncode"] != 0, f"check unexpectedly succeeded while {expected} was planted")
    rows, summary = check_rows(record)
    require(type(summary.get("errors")) is int and summary["errors"] > 0,
            f"check summary did not count an error for {expected}: {summary}")
    require(summary.get("not_checked") == [],
            f"check was incomplete for {expected}: not_checked={summary.get('not_checked')!r}")
    require(diagnostic_matches(rows, expected),
            f"check JSON has no error finding carrying {expected}")


def perform_error(root: Path, case: dict, step: dict, given: Input, env: dict[str, str],
                  log: CommandLog) -> None:
    args = target_args(root, case, step)
    record = log([given.blaze, "run", *args, *worker_args(given)], root, env, given.timeout,
                 "run-expected-refusal")
    require(not record["timed_out"], "run for the expected CLI refusal timed out")
    require(record["returncode"] != 0, "the expected target/feature refusal unexpectedly ran successfully")
    output = record["stdout"] + "\n" + record["stderr"]
    require(step["error"] in output,
            f"refusal output does not contain {step['error']!r}; see {record.get('label', 'command evidence')}")


def exact_lines(stdout: str) -> list[str]:
    return stdout.splitlines()


def case_exports_artifacts(case: dict) -> bool:
    return any(step.get("replay") for step in steps_for(case))


def perform_prints(root: Path, case: dict, step: dict, given: Input, env: dict[str, str],
                   log: CommandLog, state: dict) -> None:
    if case_exports_artifacts(case):
        export_and_run(root, case, step, given, env, log, state, "initial")
        return
    args = target_args(root, case, step)
    record = log([given.blaze, "run", *args, *worker_args(given)], root, env, given.timeout,
                 "run-printing-program")
    require(not record["timed_out"], "blaze run timed out")
    require(record["returncode"] == 0,
            f"blaze run exited {record['returncode']}: {record['stderr'][-800:]}")
    require(exact_lines(record["stdout"]) == step["prints"],
            f"stdout lines {exact_lines(record['stdout'])!r} differ from expected {step['prints']!r}")


def parse_export_stats(record: dict) -> dict[str, int]:
    for line in reversed(record["stderr"].splitlines()):
        if line.startswith("export stats: "):
            values = {}
            for part in line.removeprefix("export stats: ").split():
                if "=" in part:
                    key, value = part.split("=", 1)
                    if key != "wall" and value.isdigit():
                        values[key] = int(value)
            if values:
                return values
    raise CaseFailed("export --stats produced no `export stats:` line")


def artifact_hashes(directory: Path) -> dict[str, str]:
    if not directory.is_dir():
        raise CaseFailed(f"artifact output directory {directory} does not exist")
    return {str(path.relative_to(directory)): digest(path)
            for path in sorted(directory.rglob("*")) if path.is_file() and not path.is_symlink()}


def export_args(given: Input, package: str, binary: str | None, output: Path,
                extra: list[str], library: bool = False) -> list[str]:
    args = [given.blaze, "export", "-p", package]
    if library:
        if binary is not None:
            raise CaseFailed("cdylib export target selection is both --lib and --bin")
        args += ["--lib"]
    elif binary is not None:
        args += ["--bin", binary]
    return [*args, *extra, "-o", str(output), *worker_args(given), "--stats"]


def capture_profile(root: Path, log: CommandLog, label: str) -> None:
    for name in ("profile.txt", "start.txt"):
        source = root / ".blaze/v2" / name
        if source.is_file() and not source.is_symlink():
            shutil.copy2(source, log.directory / f"{label}-{name}")


def export_one(given: Input, root: Path, env: dict[str, str], log: CommandLog,
               package: str, binary: str | None, output: Path, extra: list[str], label: str,
               library: bool = False) -> tuple[dict, dict[str, int] | None]:
    record = log(export_args(given, package, binary, output, extra, library), root, env,
                 given.timeout, label)
    capture_profile(root, log, label)
    stats = None
    if not record["timed_out"] and record["returncode"] == 0:
        stats = parse_export_stats(record)
    return record, stats


def cdylib_packages(root: Path) -> list[str]:
    return sorted(package for package, fact in manifests(root).items() if fact["cdylib"])


def export_and_run(root: Path, case: dict, step: dict, given: Input, env: dict[str, str],
                   log: CommandLog, state: dict, series: str) -> None:
    """Export the executable and every cdylib companion, then execute the executable.

    The two exports are intentionally independent evidence: a refused library export does not
    hide what the application export or the resulting program did.
    """
    package, binary = choose_target(root, case, step)
    extra = list(step.get("args", []))
    # `--bin` in retained cargo-style arguments is already represented in package/binary.
    filtered = []
    index = 0
    while index < len(extra):
        if extra[index] == "--bin" and index + 1 < len(extra):
            index += 2
        else:
            filtered.append(extra[index])
            index += 1
    output_names = {"initial": "export-output", "replay-unchanged": "export-output",
                    "replay-new": f"replay-output-{state.get('generation', 0)}"}
    if series not in output_names:
        raise CaseFailed(f"unknown export series {series!r}")
    outdir = root / output_names[series]
    if outdir.exists():
        if series != "replay-unchanged" or outdir.is_symlink() or not outdir.is_dir():
            raise CaseFailed(f"unexpected existing export output {outdir}")
        # Keep the output path unchanged, but require export to materialize every artifact.
        shutil.rmtree(outdir)
    outdir.mkdir()
    errors = []
    suffix = ".dylib" if sys.platform == "darwin" else ".so"

    def check_stats(name: str, stats: dict | None) -> None:
        if series != "initial" and (not stats or stats.get("compiled") != 0 or stats.get("cached", 0) <= 0):
            errors.append(f"{series} {name} must report cached functions and compile none: {stats}")

    for library in cdylib_packages(root):
        artifact = outdir / f"lib{library.replace('-', '_')}{suffix}"
        record, stats = export_one(given, root, env, log, library, None, artifact, [],
                               f"export-cdylib-{library}-{series}", library=True)
        if record["timed_out"] or record["returncode"] != 0:
            errors.append(f"export of cdylib package {library} failed with exit {record['returncode']} "
                          f"timed_out={record['timed_out']}")
        elif not artifact.is_file() or artifact.is_symlink():
            errors.append(f"export of cdylib package {library} succeeded but did not write {artifact.name}")
        else:
            check_stats(library, stats)
    executable = outdir / "program"
    record, stats = export_one(given, root, env, log, package, binary, executable, filtered,
                               f"export-program-{series}")
    if record["timed_out"]:
        errors.append("program export timed out")
    elif record["returncode"] != 0:
        errors.append(f"program export exited {record['returncode']}")
    else:
        check_stats(package, stats)
    if executable.is_symlink() or not executable.is_file() or not os.access(executable, os.X_OK):
        errors.append(f"program export did not produce a regular executable at {executable}")
    hashes = artifact_hashes(outdir)
    save(log.directory / f"{state.get('generation', 0)}-{series}-artifacts.json", hashes)
    if series == "initial":
        state["artifacts"] = hashes
    else:
        difference = artifact_identity_diff(state["artifacts"], hashes)
        if difference:
            errors.append(f"{series} artifact identity differs from the initial export: {difference}")
    if errors:
        raise CaseFailed("; ".join(errors))
    ran = log([executable], root, env, min(60, given.timeout), "run-exported-program")
    require(ran["returncode"] == 0, f"exported program exited {ran['returncode']}")
    require(exact_lines(ran["stdout"]) == step["prints"],
            f"exported program stdout lines {exact_lines(ran['stdout'])!r} differ from {step['prints']!r}")


def tree_hashes(root: Path) -> dict[str, str]:
    return {str(path.relative_to(root)): digest(path)
            for path in sorted(root.rglob("*")) if path.is_file() and not path.is_symlink()}


def tree_identity(hashes: dict[str, str]) -> str:
    encoded = json.dumps(hashes, sort_keys=True, separators=(",", ":")).encode()
    return hashlib.sha256(encoded).hexdigest()


def clone_store(initial_home: Path, replay_home: Path) -> dict:
    source = initial_home / ".blaze/v2/store"
    if not source.is_dir():
        raise CaseFailed("the initial HOME has no .blaze/v2/store to replay")
    source_cas = source / "f5s3"
    if not source_cas.is_dir():
        raise CaseFailed("the initial HOME has no .blaze/v2/store/f5s3 CAS to replay")
    destination = replay_home / ".blaze/v2/store"
    destination.parent.mkdir(parents=True, exist_ok=True)
    shutil.copytree(source, destination)
    before = tree_hashes(source)
    after = tree_hashes(destination)
    if before != after:
        raise CaseFailed("the copied replay store is not byte-identical to the initial store")
    cas_before = tree_hashes(source_cas)
    cas_after = tree_hashes(destination / "f5s3")
    if not cas_before:
        raise CaseFailed("the initial f5s3 CAS has no files to replay")
    if cas_before != cas_after:
        raise CaseFailed("the copied replay f5s3 CAS bytes are not byte-identical to the initial CAS")
    for relative in before:
        if os.path.samefile(source / relative, destination / relative):
            raise CaseFailed(f"replay store file {relative} is the same inode, not an independent copy")
    return {"store_files": len(before), "store_sha256": tree_identity(before),
            "f5s3_files": len(cas_before), "f5s3_sha256": tree_identity(cas_before)}


def perform_test(root: Path, case: dict, step: dict, given: Input, env: dict[str, str],
                 log: CommandLog) -> None:
    record = log([given.blaze, "test", *worker_args(given), "--", "--format", "json",
                  "--run-all", "--exact", step["test"]], root, env, given.timeout,
                 "test-selected-exactly")
    require(not record["timed_out"], "blaze test timed out")
    require(record["returncode"] == 0, f"blaze test exited {record['returncode']}: {record['stderr'][-800:]}")
    events = []
    for line in record["stdout"].splitlines():
        if line.lstrip().startswith("{"):
            try:
                events.append(json.loads(line))
            except json.JSONDecodeError as error:
                raise CaseFailed(f"blaze test JSON output is malformed: {error}") from error
    tests = [row for row in events if row.get("event") == "test"]
    summaries = [row for row in events if row.get("event") == "summary"]
    require(len(tests) == 1, f"exactly one selected test must have a result; saw {len(tests)}")
    test = tests[0]
    path = test.get("path") if isinstance(test.get("path"), str) else ""
    require(path == step["test"] or path.endswith("/" + step["test"]),
            f"selected test path {path!r} is not the requested exact test {step['test']!r}")
    require(test.get("outcome") == "passed", f"the selected test outcome is {test.get('outcome')!r}, not passed")
    require(len(summaries) == 1, "blaze test produced no exactly-one JSON summary")
    summary = summaries[0]
    require(summary.get("ran") == 1 and summary.get("passed") == 1 and summary.get("failed") == 0,
            f"test summary does not show exactly one passed execution: {summary}")
    require(summary.get("reused") == 0,
            f"the selected test was reused rather than executed: reused={summary.get('reused')!r}")


def start_replay_home(given: Input, root: Path, log: CommandLog, state: dict,
                      initial_home: Path, scratch_case: Path, label: str) -> tuple[Path, dict[str, str], CheckoutDaemon]:
    label = f"{state['generation']}-{label}"
    replay_home = scratch_case / f"{label}-home-files"
    if replay_home.exists():
        raise CaseFailed(f"{label} HOME already exists; evidence is never overwritten")
    copy_shelf(Path(given.shelf), replay_home)
    store_evidence = clone_store(initial_home, replay_home)
    logical, container = logical_home(replay_home)
    state.setdefault("home_containers", []).append(container)
    entries = {path.name for path in (replay_home / ".blaze/v2").iterdir()}
    require(entries == {"store", "toolchain"},
            f"fresh {label} HOME starts with entries {sorted(entries)!r}, not only store and toolchain")
    replay_env = environment(logical)
    replay_daemon = CheckoutDaemon(Path(given.blaze), root, logical, replay_env, log)
    state.setdefault("daemons", []).append(replay_daemon)
    replay_daemon.start()
    (log.directory / f"{label}-store.json").write_text(json.dumps(store_evidence, indent=2) + "\n")
    return replay_home, replay_env, replay_daemon


def artifact_identity_diff(initial: dict[str, str], replay: dict[str, str]) -> str:
    missing = sorted(set(initial) - set(replay))
    extra = sorted(set(replay) - set(initial))
    changed = sorted(name for name in set(initial) & set(replay) if initial[name] != replay[name])
    parts = []
    if missing:
        parts.append(f"missing {missing!r}")
    if extra:
        parts.append(f"extra {extra!r}")
    if changed:
        parts.append(f"changed bytes {changed!r}")
    return "; ".join(parts)


def perform_replay(root: Path, case: dict, step: dict, given: Input, log: CommandLog,
                   state: dict, daemon: CheckoutDaemon, scratch_case: Path) -> CheckoutDaemon:
    daemon.stop()
    initial_home = state["physical_home"]
    state["generation"] = state.get("generation", 0) + 1
    errors = []
    for series in ("replay-new", "replay-unchanged"):
        replay_home, replay_env, replay_daemon = start_replay_home(
            given, root, log, state, initial_home, scratch_case, series)
        try:
            export_and_run(root, case, step, given, step_environment(replay_env, step), log, state, series)
            replay_daemon.check_alive()
        except CaseFailed as error:
            errors.append(f"{series}: {error}")
        if series == "replay-new":
            replay_daemon.stop()
    if errors:
        raise CaseFailed("; ".join(errors))
    state["physical_home"] = replay_home
    return replay_daemon


def step_environment(base: dict[str, str], step: dict) -> dict[str, str]:
    env = dict(base)
    for name, value in step.get("env", {}).items():
        if value is None:
            env.pop(name, None)
        else:
            env[name] = value
    return env


def perform_step(root: Path, case: dict, step: dict, given: Input, log: CommandLog,
                 state: dict, daemon: CheckoutDaemon, scratch_case: Path) -> CheckoutDaemon:
    daemon.check_alive()
    write_files(root, step.get("files", {}))
    step_env = step_environment(daemon.env, step)
    if "diagnostic" in step:
        perform_diagnostic(given, root, step_env, log, step["diagnostic"])
    if "error" in step:
        perform_error(root, case, step, given, step_env, log)
    if "test" in step:
        perform_test(root, case, step, given, step_env, log)
    if "prints" in step and not step.get("replay"):
        perform_prints(root, case, step, given, step_env, log, state)
    for name in step.get("absent", []):
        perform_absent(given, root, step_env, log, name)
    if step.get("replay"):
        daemon = perform_replay(root, case, step, given, log, state, daemon, scratch_case)
    daemon.check_alive()
    return daemon


def run_case(case: dict, given: Input, scratch: Path) -> dict:
    name = case["name"]
    case_root = scratch / "cases" / name
    root = case_root / "project"
    home = case_root / "home-files"
    log = CommandLog(scratch, name)
    result = {"ok": False, "fixes": case["fixes"], "why": "not completed", "sources": {}, "steps": []}
    state: dict = {"physical_home": home, "home_containers": [], "daemons": []}
    try:
        root.mkdir(parents=True)
        copy_shelf(Path(given.shelf), home)
        logical, container = logical_home(home)
        state["home_containers"].append(container)
        env = environment(logical)
        result["sources"] = write_files(root, initial_files(case))
        daemon = CheckoutDaemon(Path(given.blaze), root, logical, env, log)
        state["daemons"].append(daemon)
        daemon.start()
        steps = steps_for(case)
        for index, step in enumerate(steps):
            try:
                daemon = perform_step(root, case, step, given, log, state, daemon, case_root)
                result["steps"].append({"index": index, "ok": True})
            except (CaseFailed, Failed) as error:
                result["steps"].append({"index": index, "ok": False, "why": str(error)})
                raise
            finally:
                changed = {path: digest(root / path) for path in step.get("files", {}) if (root / path).is_file()}
                result["sources"].update(changed)
                if changed and len(result["steps"]) > index:
                    result["steps"][index]["sources"] = changed
        result["ok"] = True
        result["why"] = "as stated"
    except CaseFailed as error:
        result["why"] = str(error)
    except (Failed, OSError, subprocess.SubprocessError) as error:
        result["why"] = str(error)
        result["errors"] = [str(error)]
    finally:
        for owned in reversed(state["daemons"]):
            try:
                owned.stop()
            except (Failed, OSError, subprocess.SubprocessError) as error:
                result.setdefault("errors", []).append(f"cleanup: {error}")
        for container in state["home_containers"]:
            try:
                cleanup_logical_home(container)
            except OSError as error:
                result.setdefault("errors", []).append(f"cleanup logical HOME: {error}")
        if result.get("errors"):
            if result["ok"]:
                result["why"] = "; ".join(result["errors"])
            result["ok"] = False
    return result


def main(given: Input) -> int:
    given = validate(given, os.environ.get("HOME"))
    scratch = Path(given.scratch)
    scratch.mkdir(parents=True, exist_ok=True)
    warnings = []
    if given.stall_bound is not None:
        warnings.append("stall_bound is a legacy harness input: the maintained checkout daemon exposes no such tuning; value ignored")
    report = {"schema": 2, "input": {
        "blaze": given.blaze, "scratch": given.scratch, "shelf": given.shelf,
        "only": list(given.only), "timeout": given.timeout, "source": given.source,
        "workers": given.workers, "stall_bound": given.stall_bound,
    }, "warnings": warnings, "errors": [], "complete": False, "cases": {}}
    selected = [case for case in CASES if not given.only or case["name"] in given.only]
    results = {case["name"]: {"ok": False, "fixes": case["fixes"], "why": "not run"} for case in selected}
    try:
        save(scratch / "results.json", results)
        save(scratch / "run.json", report)
        before = provenance(given)
        save(scratch / "provenance.json", before)
        for case in selected:
            started = time.monotonic()
            result = run_case(case, given, scratch)
            result["seconds"] = round(time.monotonic() - started, 3)
            results[case["name"]] = result
            report["cases"][case["name"]] = {"ok": result["ok"], "why": result["why"]}
            save(scratch / "results.json", results)
            print(f"{case['name']}: {'ok' if result['ok'] else 'FAILED'}: {result['why']}", flush=True)
            if result.get("errors"):
                report["errors"].extend(result["errors"])
                break
        after = provenance(given)
        save(scratch / "provenance-after.json", after)
        if after != before:
            report["errors"].append("source, harness, shelf or binary provenance changed during acceptance")
        report["complete"] = not report["errors"]
    except (Failed, OSError, subprocess.SubprocessError) as error:
        report["errors"].append(str(error))
    finally:
        save(scratch / "run.json", report)
    failed = sum(1 for result in results.values() if not result["ok"])
    for error in report["errors"]:
        print(f"fork migration: {error}", file=sys.stderr)
    print(f"fork migration: {len(results) - failed}/{len(results)} cases passed; evidence in {scratch}", flush=True)
    return 1 if failed or report["errors"] else 0


if __name__ == "__main__":
    if len(sys.argv) != 2:
        sys.exit(__doc__)
    try:
        sys.exit(main(parse(Input, sys.argv[1], "forkfixes.py")))
    except (OSError, ValueError, TypeError) as error:
        sys.exit(f"forkfixes.py: {error}")
