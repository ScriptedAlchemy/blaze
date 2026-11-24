"""report.py: the retained shape of an objective acceptance result.

A script makes a Report, adds an Outcome for each check, and calls finish(), which writes
the JSON result and exits. Failed, erroneous, unmeasured and empty runs all fail. The
objective ledger consumes this schema; a report is evidence only for the checks it ran.

The engine descriptor records the requested client and backends. It starts no compiler
and supplies no execution driver. Source and binary identity remain the caller's checks.
"""

import json
import os
import platform
import subprocess
import sys
import time
from collections import Counter
from collections.abc import Callable
from dataclasses import asdict, dataclass, field
from pathlib import Path
from typing import Literal, NoReturn

ROOT = Path(__file__).resolve().parents[2]
type Kind = Literal["v2"]
type Backend = Literal["cranelift", "llvm"]
type DevBackend = Literal["default", "stencil", "cranelift"]
DEV_BACKENDS: tuple[DevBackend, ...] = ("default", "stencil", "cranelift")
type Verdict = Literal["pass", "fail", "unmeasured", "error"]
type Number = int | float | str | bool | None


class Unmeasurable(Exception):
    """The requested check has no measured answer."""


class Failed(Exception):
    """The harness could not complete or interpret a check."""


@dataclass(frozen=True)
class EngineSpec:
    """The existing report descriptor, independent of the deleted daemon driver."""

    kind: Kind
    blaze: str
    backend: Backend = "cranelift"
    dev_backend: DevBackend = "default"

    def __post_init__(self) -> None:
        if self.dev_backend not in DEV_BACKENDS:
            raise Failed(f"engine.dev_backend: one of {', '.join(DEV_BACKENDS)}, not {self.dev_backend!r}")


@dataclass(frozen=True)
class Outcome:
    name: str
    verdict: Verdict
    detail: str = ""
    numbers: dict[str, Number] = field(default_factory=dict[str, Number])


def commit() -> str | None:
    ran = subprocess.run(["git", "-C", str(ROOT), "rev-parse", "HEAD"],
                         capture_output=True, text=True, check=False, timeout=30)
    return ran.stdout.strip() or None


def host() -> dict[str, Number]:
    """What the run was on."""
    affinity: Callable[[int], set[int]] | None = getattr(os, "sched_getaffinity", None)
    cores = len(affinity(0)) if affinity is not None else os.cpu_count()
    memory_gib: float | None = None
    try:
        with open("/proc/meminfo") as f:
            kb = int(next(line for line in f if line.startswith("MemTotal:")).split()[1])
        memory_gib = round(kb / 2**20, 1)
    except (OSError, StopIteration, ValueError):
        if sys.platform == "darwin":
            out = subprocess.run(["sysctl", "-n", "hw.memsize"], capture_output=True,
                                 text=True, check=False, timeout=30).stdout.strip()
            memory_gib = round(int(out) / 2**30, 1) if out.isdigit() else None
    return {"os": platform.system(), "arch": platform.machine(), "kernel": platform.release(), "cores": cores,
            "machine_cores": os.cpu_count(), "memory_gib": memory_gib, "name": os.environ.get("BLAZE_BENCH_MACHINE")}


def now() -> str:
    return time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())


class Report:
    def __init__(self, script: str, engine: EngineSpec, out: str):
        self.script, self.engine = script, engine
        self.out = Path(out) if out else None
        self.started = now()
        self.outcomes: list[Outcome] = []
        self.extra: dict[str, object] = {}

    def add(self, name: str, verdict: Verdict, detail: str = "", **numbers: Number) -> Outcome:
        outcome = Outcome(name, verdict, detail, numbers)
        self.outcomes.append(outcome)
        shown = "  ".join(f"{k}={v}" for k, v in numbers.items())
        first = detail.splitlines()[0][:220] if detail else ""
        print(f"{verdict.upper():10} {name}  {shown}  {first}".rstrip(), flush=True)
        return outcome

    def guarded[T](self, name: str, fn: Callable[[], T]) -> T | None:
        """Record an unmeasured or erroneous check; neither can make the report pass."""
        try:
            return fn()
        except Unmeasurable as e:
            self.add(name, "unmeasured", str(e))
        except Failed as e:
            self.add(name, "error", str(e))
        return None

    def failing(self) -> list[Outcome]:
        bad: set[Verdict] = {"fail", "error", "unmeasured"}
        return [o for o in self.outcomes if o.verdict in bad]

    def finish(self) -> NoReturn:
        # Empty selections and loops over no case are errors, never successful evidence.
        if not self.outcomes:
            self.add("checked nothing", "error", f"{self.script} ended with no outcome: nothing it was asked to check ran")
        counts = Counter(o.verdict for o in self.outcomes)
        result = {"script": self.script, "engine": asdict(self.engine), "host": host(), "commit": commit(),
                  "started": self.started, "ended": now(), "outcomes": [asdict(o) for o in self.outcomes],
                  "counts": dict(counts), "extra": self.extra}
        if self.out:
            self.out.parent.mkdir(parents=True, exist_ok=True)
            self.out.write_text(json.dumps(result, indent=1) + "\n")
        bad = self.failing()
        print(f"{self.script}: " + ", ".join(f"{n} {v}" for v, n in sorted(counts.items()))
              + (f"; result {self.out}" if self.out else ""), flush=True)
        sys.exit(1 if bad else 0)
