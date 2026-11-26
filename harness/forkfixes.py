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
    source   a checkout whose HEAD is recorded as provenance (optional)
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
is used. Unsupported evidence is never treated as a pass. A case failure does not stop
independent later cases.
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

MAIN = "src/main.rs"


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


def digest(path: Path) -> str:
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def require(condition: bool, message: str) -> None:
    if not condition:
        raise CaseFailed(message)


def validate(given: Input, harness_home: str | None) -> Input:
    if type(given.timeout) is not int or given.timeout <= 0:
        raise Failed("timeout must be a positive integer")
    if given.workers is not None and (type(given.workers) is not int or given.workers <= 0):
        raise Failed("workers must be a positive integer")
    if given.stall_bound is not None and type(given.stall_bound) is not int:
        raise Failed("stall_bound must be an integer when given")
    blaze = Path(given.blaze)
    if blaze.is_symlink() or not blaze.is_file() or not os.access(blaze, os.X_OK):
        raise Failed(f"blaze {blaze} is not a regular executable")
    scratch = Path(given.scratch)
    if scratch.is_symlink() or (scratch.exists() and any(scratch.iterdir())):
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
    unknown = sorted(set(given.only) - {case["name"] for case in CASES})
    if unknown:
        raise Failed(f"unknown case names in only: {', '.join(unknown)}")
    return Input(str(blaze.absolute()), str(scratch.absolute()), str(shelf_path.resolve()),
                 given.only, given.timeout, given.source, given.workers, given.stall_bound)


def binary_digests(blaze: Path) -> dict[str, str]:
    result = {"blaze": digest(blaze)}
    for name in ("blaze-launcher", "blaze-fact-shim"):
        path = blaze.with_name(name)
        if path.is_file() and not path.is_symlink():
            result[name] = digest(path)
    return result


def provenance(given: Input) -> dict:
    root = Path(__file__).resolve().parents[1]
    files = {}
    for name in ("harness/forkfixes.py", "harness/forkfixes/cases.py",
                 ".github/scripts/forkfixes_acceptance.py"):
        path = root / name
        if path.is_file():
            files[name] = digest(path)
    answer = {"binaries": binary_digests(Path(given.blaze)), "harness": files,
              "shelf": shelf_provenance(Path(given.shelf))}
    if given.source:
        try:
            head = subprocess.run(["git", "-C", given.source, "rev-parse", "HEAD"],
                                  capture_output=True, text=True, timeout=10, check=False)
            answer["source_head"] = head.stdout.strip() if head.returncode == 0 else f"unavailable: {head.stderr.strip()}"
        except (OSError, subprocess.SubprocessError) as error:
            answer["source_head"] = f"unavailable: {error}"
    return answer


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
        "PYTHONDONTWRITEBYTECODE": "1",
    })
    for key in ("BLAZE_CORES", "BLAZE_TESTS", "BLAZE_STORE_MAX_BYTES", "BLAZE_STORE_MAX_GB"):
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
    if exact:
        return exact[0]
    # A simple fixture's generated package has underscores while its retained program key may
    # be the user-facing binary name; if it is the sole binary, that binary is the target.
    if len(binaries) == 1:
        return binaries[0]
    raise CaseFailed(f"no binary target {requested!r} in fixture {case['name']}")


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


def run_command(argv: list[str], cwd: Path, env: dict[str, str], timeout: int,
                evidence: Path, label: str) -> dict:
    """Run one bounded command in its own process group and preserve its exact output."""
    child = subprocess.Popen([str(part) for part in argv], cwd=cwd, env=env,
                             stdin=subprocess.DEVNULL, stdout=subprocess.PIPE,
                             stderr=subprocess.PIPE, text=True, errors="replace",
                             start_new_session=True)
    expired = False
    stdout = stderr = ""
    try:
        stdout, stderr = child.communicate(timeout=timeout)
    except subprocess.TimeoutExpired:
        expired = True
        try:
            os.killpg(child.pid, signal.SIGTERM)
        except ProcessLookupError:
            pass
        try:
            stdout, stderr = child.communicate(timeout=10)
        except subprocess.TimeoutExpired:
            os.killpg(child.pid, signal.SIGKILL)
            stdout, stderr = child.communicate(timeout=10)
    finally:
        if child.returncode is None:
            try:
                os.killpg(child.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            child.wait(timeout=10)
    record = {"label": label, "argv": [str(part) for part in argv], "cwd": str(cwd),
              "returncode": child.returncode, "timed_out": expired,
              "stdout": stdout, "stderr": stderr}
    evidence.parent.mkdir(parents=True, exist_ok=True)
    evidence.write_text(json.dumps(record, indent=2) + "\n")
    return record


class CommandLog:
    def __init__(self, root: Path, case: str):
        self.directory = root / "evidence" / case
        self.count = 0

    def __call__(self, argv: list[str], cwd: Path, env: dict[str, str], timeout: int,
                 label: str) -> dict:
        self.count += 1
        safe = re.sub(r"[^A-Za-z0-9_.-]+", "-", label)
        return run_command(argv, cwd, env, timeout, self.directory / f"{self.count:03d}-{safe}.json", label)


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

    def status(self) -> tuple[int, str, str]:
        try:
            result = subprocess.run([str(self.blaze), "daemon", "--status"], cwd=self.root,
                                    env=self.env, stdin=subprocess.DEVNULL, capture_output=True,
                                    text=True, timeout=10, check=False)
            return result.returncode, result.stdout, result.stderr
        except (OSError, subprocess.SubprocessError) as error:
            return 127, "", str(error)

    def start(self) -> None:
        stream = self.output.open("ab")
        self.process = subprocess.Popen([str(self.blaze), "daemon"], cwd=self.root,
                                        env=self.env, stdin=subprocess.DEVNULL,
                                        stdout=stream, stderr=subprocess.STDOUT,
                                        start_new_session=True)
        deadline = time.monotonic() + 30
        attempts = []
        while time.monotonic() < deadline:
            code, stdout, stderr = self.status()
            attempts.append({"returncode": code, "stdout": stdout, "stderr": stderr})
            if self.process.poll() is not None:
                break
            if code == 0 and daemon_status_pid(stdout) == self.process.pid:
                (self.log.directory / "daemon-start.json").write_text(json.dumps(attempts, indent=2) + "\n")
                return
            time.sleep(0.25)
        detail = f"checkout daemon did not become ready for {self.root}; "
        if self.process.poll() is not None:
            detail += f"it exited {self.process.returncode}"
        else:
            detail += "status never reported its pid"
        (self.log.directory / "daemon-start.json").write_text(json.dumps(attempts, indent=2) + "\n")
        raise CaseFailed(detail)

    def check_alive(self) -> None:
        if self.process is None or self.process.poll() is not None:
            raise CaseFailed("the owned checkout daemon exited during the case")
        code, stdout, stderr = self.status()
        if code != 0 or daemon_status_pid(stdout) != self.process.pid:
            raise CaseFailed(f"checkout daemon status did not report the owned pid {self.process.pid}: "
                             f"exit {code}, stdout {stdout.strip()!r}, stderr {stderr.strip()!r}")

    def stop(self) -> None:
        if self.process is None:
            return
        errors = []
        if self.process.poll() is None:
            record = self.log([str(self.blaze), "daemon", "--stop"], self.root, self.env, 20, "daemon-stop")
            if record["timed_out"] or record["returncode"] != 0:
                errors.append(f"daemon --stop returned {record['returncode']} timed_out={record['timed_out']}")
            try:
                self.process.wait(timeout=20)
            except subprocess.TimeoutExpired:
                errors.append("checkout daemon did not exit after daemon --stop")
                try:
                    os.killpg(self.process.pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass
                self.process.wait(timeout=10)
            if self.process.returncode != 0:
                errors.append(f"checkout daemon exited {self.process.returncode} despite daemon --stop")
        if errors:
            self.process = None
            raise CaseFailed("; ".join(errors))
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
            if code == expected or expected in message:
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
                   log: CommandLog, state: dict, series: str) -> dict[str, int]:
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
                    "replay-new": "replay-output"}
    if series not in output_names:
        raise CaseFailed(f"unknown export series {series!r}")
    outdir = root / output_names[series]
    if outdir.exists():
        if series == "replay-unchanged":
            if not outdir.is_dir():
                raise CaseFailed(f"unchanged export output {outdir} exists and is not a directory")
        else:
            shutil.rmtree(outdir)
    outdir.mkdir(exist_ok=True)
    errors = []
    for library in cdylib_packages(root):
        artifact = outdir / f"lib{library.replace('-', '_')}.dylib"
        record, _ = export_one(given, root, env, log, library, None, artifact, [],
                               f"export-cdylib-{library}-{series}", library=True)
        if record["timed_out"] or record["returncode"] != 0:
            errors.append(f"export of cdylib package {library} failed with exit {record['returncode']} "
                          f"timed_out={record['timed_out']}")
        elif not artifact.is_file():
            errors.append(f"export of cdylib package {library} succeeded but did not write {artifact.name}")
    executable = outdir / "program"
    record, stats = export_one(given, root, env, log, package, binary, executable, filtered,
                               f"export-program-{series}")
    if record["timed_out"]:
        errors.append("program export timed out")
    elif record["returncode"] != 0:
        errors.append(f"program export exited {record['returncode']}")
    if executable.is_file():
        if not os.access(executable, os.X_OK):
            errors.append("exported program is not executable")
        else:
            ran = log([executable], root, env, min(60, given.timeout), "run-exported-program")
            if ran["timed_out"] or ran["returncode"] != 0:
                errors.append(f"exported program exited {ran['returncode']} timed_out={ran['timed_out']}")
            elif exact_lines(ran["stdout"]) != step["prints"]:
                errors.append(f"exported program stdout lines {exact_lines(ran['stdout'])!r} differ from {step['prints']!r}")
    else:
        errors.append("program export produced no executable")
    hashes = artifact_hashes(outdir)
    artifact_keys = {"initial": "artifacts", "replay-unchanged": "replay_artifacts_unchanged",
                     "replay-new": "replay_artifacts_new"}
    state[artifact_keys[series]] = hashes
    for library in cdylib_packages(root):
        name = f"lib{library.replace('-', '_')}.dylib"
        if name not in hashes:
            errors.append(f"artifact set lacks companion cdylib {name}")
    if errors:
        raise CaseFailed("; ".join(errors))
    if stats is None:
        raise CaseFailed("successful program export had no parsed stats")
    return stats


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


def perform_replay(root: Path, case: dict, step: dict, given: Input, env: dict[str, str],
                   log: CommandLog, state: dict, daemon: CheckoutDaemon,
                   home: Path, scratch_case: Path) -> CheckoutDaemon:
    daemon.stop()
    initial_home = Path(state["physical_home"])
    attempts: dict[str, dict] = {}
    errors = []
    for series, label in (("replay-unchanged", "replay-unchanged"), ("replay-new", "replay-new")):
        attempt = {"label": label, "stats": None, "daemon": None, "home": None, "error": None}
        attempts[series] = attempt
        try:
            replay_home, replay_env, replay_daemon = start_replay_home(
                given, root, log, state, initial_home, scratch_case, label)
            attempt.update({"home": replay_home, "daemon": replay_daemon})
            try:
                attempt["stats"] = export_and_run(root, case, step, given, replay_env, log, state, series)
            except Exception as error:
                attempt["error"] = str(error)
        except Exception as error:
            attempt["error"] = str(error)
        if attempt["error"]:
            errors.append(f"{series}: {attempt['error']}")

    for series, key in (("replay-unchanged", "replay_artifacts_unchanged"),
                        ("replay-new", "replay_artifacts_new")):
        stats = attempts[series]["stats"]
        if stats is not None:
            if stats.get("cached", 0) <= 0:
                errors.append(f"{series} reused no compiled functions: {stats}")
            if stats.get("compiled") != 0:
                errors.append(f"{series} compiled functions instead of replaying them: {stats}")
        initial = state.get("artifacts")
        replayed = state.get(key)
        if initial is not None and replayed is not None and initial != replayed:
            errors.append(f"{series} artifact identity differs from the initial export: "
                          f"{artifact_identity_diff(initial, replayed)}")

    supplemental = attempts["replay-new"]["daemon"]
    if supplemental is not None:
        try:
            supplemental.stop()
        except CaseFailed as error:
            errors.append(f"replay-new daemon cleanup: {error}")
    if errors:
        raise CaseFailed("; ".join(errors))
    canonical = attempts["replay-unchanged"]
    state["physical_home"] = canonical["home"]
    return canonical["daemon"]

def step_environment(base: dict[str, str], step: dict) -> dict[str, str]:
    env = dict(base)
    for name, value in step.get("env", {}).items():
        if value is None:
            env.pop(name, None)
        else:
            env[name] = value
    return env


def perform_step(root: Path, case: dict, step: dict, given: Input, env: dict[str, str],
                 log: CommandLog, state: dict, daemon: CheckoutDaemon,
                 home: Path, scratch_case: Path) -> CheckoutDaemon:
    daemon.check_alive()
    write_files(root, step.get("files", {}))
    step_env = step_environment(env, step)
    if "diagnostic" in step:
        perform_diagnostic(given, root, step_env, log, step["diagnostic"])
    if "error" in step:
        perform_error(root, case, step, given, step_env, log)
    if "test" in step:
        perform_test(root, case, step, given, step_env, log)
    if "prints" in step:
        perform_prints(root, case, step, given, step_env, log, state)
    for name in step.get("absent", []):
        perform_absent(given, root, step_env, log, name)
    if step.get("replay"):
        daemon = perform_replay(root, case, step, given, env, log, state, daemon, home, scratch_case)
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
    daemon = None
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
            before = dict(result["sources"])
            result["sources"].update({path: hashlib.sha256(content.encode()).hexdigest()
                                      for path, content in step.get("files", {}).items()})
            try:
                daemon = perform_step(root, case, step, given, env, log, state, daemon, home, case_root)
                result["steps"].append({"index": index, "ok": True})
            except CaseFailed as error:
                result["steps"].append({"index": index, "ok": False, "why": str(error)})
                raise
            finally:
                if before != result["sources"]:
                    state["mutated"] = sorted(set(result["sources"]) - set(before))
        result["ok"] = True
        result["why"] = "as stated"
    except Exception as error:
        result["ok"] = False
        result["why"] = str(error)
    finally:
        for owned in reversed(state.get("daemons", [])):
            try:
                owned.stop()
            except CaseFailed as error:
                result["ok"] = False
                result["why"] += f"; cleanup: {error}"
        for container in state["home_containers"]:
            try:
                cleanup_logical_home(container)
            except OSError as error:
                result["ok"] = False
                result["why"] += f"; cleanup logical HOME: {error}"
    return result


def main(given: Input) -> int:
    scratch = Path(given.scratch)
    scratch.mkdir(parents=True, exist_ok=True)
    before = provenance(given)
    (scratch / "provenance.json").write_text(json.dumps(before, indent=2) + "\n")
    warnings = []
    if given.stall_bound is not None:
        warnings.append("stall_bound is a legacy harness input: the maintained checkout daemon exposes no such tuning; value ignored")
    report = {"schema": 2, "input": {
        "blaze": given.blaze, "scratch": given.scratch, "shelf": given.shelf,
        "only": list(given.only), "timeout": given.timeout, "source": given.source,
        "workers": given.workers, "stall_bound": given.stall_bound,
    }, "warnings": warnings, "errors": [], "complete": False, "cases": {}}
    selected = [case for case in CASES if not given.only or case["name"] in given.only]
    results = {}
    try:
        for case in selected:
            started = time.monotonic()
            result = run_case(case, given, scratch)
            result["seconds"] = round(time.monotonic() - started, 3)
            results[case["name"]] = result
            report["cases"][case["name"]] = {"ok": result["ok"], "why": result["why"]}
            (scratch / "results.json").write_text(json.dumps(results, indent=2) + "\n")
            print(f"{case['name']}: {'ok' if result['ok'] else 'FAILED'}: {result['why']}", flush=True)
        after = provenance(given)
        (scratch / "provenance-after.json").write_text(json.dumps(after, indent=2) + "\n")
        if after != before:
            report["errors"].append("source, harness, shelf or binary provenance changed during the run")
        report["complete"] = not report["errors"]
    except BaseException as error:
        report["errors"].append(f"{type(error).__name__}: {error}")
        raise
    finally:
        (scratch / "run.json").write_text(json.dumps(report, indent=2) + "\n")
        if not (scratch / "results.json").exists():
            (scratch / "results.json").write_text(json.dumps(results, indent=2) + "\n")
    failed = sum(1 for result in results.values() if not result["ok"])
    print(f"fork migration: {len(results) - failed}/{len(results)} passed; evidence in {scratch}", flush=True)
    return 1 if failed or report["errors"] else 0


if __name__ == "__main__":
    if len(sys.argv) != 2:
        sys.exit(__doc__)
    try:
        sys.exit(main(validate(parse(Input, sys.argv[1], "forkfixes.py"), os.environ.get("HOME"))))
    except (OSError, ValueError, TypeError) as error:
        sys.exit(f"forkfixes.py: {error}")
