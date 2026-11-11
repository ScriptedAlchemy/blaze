"""distributed.py: D11's real native remote-cache acceptance, in both build profiles.

One typed JSON object (inline, a file, or `-`):
    blaze                  the source-built client, beside blaze-engine, blaze-dist-local,
                           blaze-serve and their services=true blaze-build.json seal
    out                    a new ledger report path on a reflink filesystem
    cores, daemon_memory_gb required positive integer per-daemon resource hints; the D11
                           check supplies 2 cores and 6 GiB, as the existing CI lane does
    timeout                seconds per build/fault stage (3600)
    command_timeout        seconds for status, stop and running an artifact (60)
    stall_bound            optional daemon loop-stall bound, in seconds

The existing build-identity verifier binds all four programs to the current clean source.
This adapter never builds or seals them. Evidence stays beside out in <out>.distv2, with a
fresh dev and release scratch directory. Reusing out/evidence or a volume that cannot prove
shared clone blocks is an error; pass ledger.py a new work directory on the prepared volume.

distv2.main is the acceptance authority, including its finally cleanup. Both profiles keep
every cold-cache, publication and fault oracle; release also requires real LLVM and folded
work from two holders. There is no profile filter, fault opt-out, or second receipt policy.
These checks cover native distributed-cache reuse. D11's signed global-cache integration
and off-critical-path latency/overlap evidence remain missing in the objective ledger.
"""

import contextlib
import importlib.util
import json
import signal
import sys
import traceback
from dataclasses import asdict, dataclass
from pathlib import Path
from typing import Any

HERE = Path(__file__).resolve().parent
ROOT = HERE.parents[1]
sys.path[:0] = [str(HERE), str(HERE.parent), str(ROOT / ".github/scripts")]
import clone_out
import report
from engine import EngineSpec
from typed_input import fail, parse

# The CI helper lives outside the Python workspace's import roots; its runtime import
# uses the explicit sibling path above, as other v2 adapters import legacy helpers.
identity: Any = importlib.import_module("dist_build_identity")

# `distv2` is also the package holding the existing evidence oracles. Load its CLI under
# another name so its own `from distv2 import evidence` still names that package.
spec = importlib.util.spec_from_file_location("distributed_acceptance", HERE.parent / "distv2.py")
if spec is None or spec.loader is None:
    raise RuntimeError("cannot load the distv2 acceptance harness")
distv2: Any = importlib.util.module_from_spec(spec)
sys.modules[spec.name] = distv2
spec.loader.exec_module(distv2)

SCRIPT = "distributed.py"


@dataclass(frozen=True)
class Input:
    blaze: str
    out: str
    cores: int
    daemon_memory_gb: int
    timeout: int = 3600
    command_timeout: int = 60
    stall_bound: int | None = None


def verified_build(directory: Path, initial: object | None = None) -> object:
    """Use the CI verifier, including its clean-source and exact executable-set rules."""
    sealed: object = json.loads((directory / identity.MANIFEST).read_text())
    actual: dict[str, object] = identity.identity(directory, services=True)
    identity.verify(sealed, actual)
    if initial is not None:
        # A new seal between profiles cannot replace the identity this run started with.
        identity.verify(initial, actual)
    return sealed


def require_reflink(directory: Path) -> None:
    """Reuse the clone oracle: private file, shared clone, then a private changed block."""
    if sys.platform not in ("linux", "darwin"):
        raise ValueError(f"distv2 needs Unix process controls and a reflink volume, not {sys.platform}")
    try:
        clone_out.selftest(str(directory))
    except (SystemExit, Exception) as why:
        raise ValueError(f"scratch {directory} did not prove reflink placement: {why}") from why


def profile_input(given: Input, blaze: Path, evidence: Path, name: str) -> Any:
    # Unlike the underlying standalone harness, this ledger adapter accepts no ambient
    # resource fallback: require exact positive ints here, then let the legacy loader
    # validate and construct its own Input without duplicating that schema.
    if any(type(value) is not int or value <= 0 for value in (given.cores, given.daemon_memory_gb)):
        raise ValueError("cores and daemon_memory_gb must be explicit positive integers")
    scratch = evidence / name
    return distv2.load(json.dumps({
        "blaze": str(blaze),
        "dist_local": str(blaze.parent / "blaze-dist-local"),
        "serve": str(blaze.parent / "blaze-serve"),
        "scratch": str(scratch),
        "args": ["--release"] if name == "release" else [],
        "profile": "release" if name == "release" else "debug",
        "faults": True,
        "release_fold": name == "release",
        "cores": given.cores,
        "daemon_memory_gb": given.daemon_memory_gb,
        "timeout": given.timeout,
        "command_timeout": given.command_timeout,
        "stall_bound": given.stall_bound,
        # Fixture locks are disposable. Shared std outputs stay on the same new volume;
        # every daemon still has distv2's separate BLAZE_HOME, store and target directory.
        "env": {"BLAZE_LOCK": "write", "XDG_CACHE_HOME": str(scratch / "machine-cache")},
    }))


def accept(name: str, given: Any, directory: Path, initial: object, log: Path) -> list[str]:
    errors: list[str] = []
    previous_handler = signal.getsignal(signal.SIGTERM)
    with log.open("x") as output, contextlib.redirect_stdout(output), contextlib.redirect_stderr(output):
        try:
            verified_build(directory, initial)
            returned: object = distv2.main(given)
            if returned is not None:
                raise ValueError(f"distv2.main returned {returned!r} instead of completing its checks")
        except (SystemExit, Exception) as why:
            traceback.print_exc()
            errors.append(f"{name} acceptance: {why}")
        finally:
            # main returns only after Run.stop. Even a failed main/cleanup must not hide
            # source or binary drift, or let a different identity run the next profile.
            signal.signal(signal.SIGTERM, previous_handler)
            try:
                verified_build(directory, initial)
            except (SystemExit, Exception) as why:
                traceback.print_exc()
                errors.append(f"identity after {name} cleanup: {why}")
    return errors


def main(given: Input) -> None:
    if not given.blaze or not given.out:
        fail(SCRIPT, "blaze and out must name the sealed client and a new report path")
    out = Path(given.out).resolve()
    if out.exists():
        fail(SCRIPT, f"report {out} already exists; choose a new out and keep the earlier evidence")
    blaze = Path(given.blaze).resolve()
    evidence = out.with_name(out.name + ".distv2")
    rep = report.Report(SCRIPT, EngineSpec("v2", str(blaze)), str(out))
    rep.extra["evidence"] = str(evidence)
    try:
        if blaze.name != "blaze":
            raise ValueError("blaze must be the sealed directory's blaze client")
        initial = verified_build(blaze.parent)
        try:
            evidence.mkdir(parents=True, exist_ok=False)
        except FileExistsError as why:
            raise ValueError(f"evidence {evidence} already exists; choose a new out") from why
        (evidence / "build-identity.json").write_text(json.dumps(initial, indent=2) + "\n")
        with (evidence / "reflink.log").open("x") as output, contextlib.redirect_stdout(output):
            require_reflink(evidence)
        profiles: list[tuple[str, Any]] = [
            (name, profile_input(given, blaze, evidence, name)) for name in ("dev", "release")
        ]
    except (SystemExit, Exception) as why:
        rep.add("distributed-prerequisites", "error", str(why))
        rep.finish()

    rep.extra["build_identity"] = initial
    stopped = False
    for name, profile in profiles:
        if stopped:
            rep.add(f"distv2-{name}", "error", "not run after the preceding acceptance or cleanup failed")
            continue
        (evidence / f"{name}-input.json").write_text(json.dumps(asdict(profile), indent=2) + "\n")
        log = evidence / f"{name}.log"
        print(f"{SCRIPT}: {name} complete fault acceptance; evidence {log}", flush=True)
        errors = accept(name, profile, blaze.parent, initial, log)
        if errors:
            rep.add(f"distv2-{name}", "fail", "; ".join(errors), log=str(log))
            stopped = True
        else:
            rep.add(f"distv2-{name}", "pass", "all real distv2 checks and owned-process cleanup completed",
                    log=str(log), faults=True, release_fold=name == "release")
    rep.finish()


if __name__ == "__main__":
    if len(sys.argv) != 2:
        fail(SCRIPT, f"takes one JSON value (inline, a file, or -)\n\n{__doc__}")
    main(parse(Input, sys.argv[1], SCRIPT))
