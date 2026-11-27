"""real_smoke.py: a small real workspace built by an installed blaze the way a user starts it.

Why it exists (2026-11-12, round 1 of int-1112): twice a tree that built and passed its small
tests failed a real project's cold build at its build scripts. The second time ten build scripts
of reth were refused because the embedded sandbox interposer had a RUNPATH naming blaze-cargo's
own build directory, which no hello-world sees (it has no build script). So a round's tree is
offered to nobody as "the tree" until this passes on it, and it runs before any test table
(agents/howto: real-smoke).

The workspace is `harness/v2/smoke`: crossbeam-utils and crossbeam-deque (build scripts blaze
proves pure), serde with derive and serde_json (a proc macro and a build script), getrandom,
parking_lot, and a local `-sys` crate whose build script compiles C with `cc` (no bindgen). The
program prints one line that holds the value `answer()` returns, so each step's proof is the
line of that step's sources.

    uv run --frozen python -I harness/v2/real_smoke.py '{"blaze": "/path/to/install", "work": "/path/to/smoke", "tmpfs": "", "modules": "/path/to/proc_macro_modules", "toolchain_home": "/path/to/toolchain-home"}'

The input is one JSON object (`Input`), inline, a file path, or `-` for stdin. Unknown
fields are refused. The rows with C require the pinned native C backend on macOS ARM64.

    blaze   the directory of an installed blaze (`blaze`, `blaze-launcher` and `blaze-fact-shim`
            copied out of the build tree: a blaze run from `target/` or under `cargo run`
            inherits cargo's library path and is not what a user has)
    work    a directory on a disk (refused when it is on a tmpfs, where /proc/mounts can say):
            the workspace's copy and the home are made new under it
    tmpfs   a directory on a tmpfs for the second run (outputs are placed otherwise there:
            a tmpfs has no clone); "" leaves that run out and says so
    modules the bootstrap module builder (`scripts/src/bin/proc_macro_modules.rs`, decision
            D52): the one that builds the proc-macro and build-script modules the workspace's
            lock names, from the lock's own sources, in its sandbox. Blaze never runs it; a
            module the store lacks is a finding in blaze, never a build it starts. Given, the
            modules are built once before the first step and copied into each home's store;
            "" runs without them and the rows say what blaze refused
    toolchain_home
            the home the module builder runs under: its `.rustup/toolchains` holds the pinned
            toolchain (`pins/toolchain`), whose cargo and rustc build the modules (D52). Required
            with `modules`; the builder inherits nothing else of the caller's environment
    steps   which of `cold`, `noop`, `edit`, `edition`, in that order (all by default); `edit`
            needs `cold`
    path    the PATH of the fixed environment: where `cc` is
    shelf   an installed toolchain shelf to copy into each fresh home (default ~/.blaze/v2/toolchain)
    clang_shelf
            the pinned C toolchain unpacked (D112: `<home>/.blaze/v2/toolchain/clang`, one folder per
            archive digest), copied into each fresh home beside the shelf: the build script of the
            local `-sys` crate compiles C with it. Blaze never fetches it (rule 3: each thing it
            needs and does not find is a refusal naming the path), so a home without it stops the
            C rows at `dlsym`. Checked before any row runs against pins/clang.json: the folder named
            by the host's archive digest, every member the native runs start or load
            (blaze_build_script toolchain.rs `SHELF_MEMBERS`), and each program's SHA-256 (default
            ~/.blaze/v2/toolchain/clang)
    workers optional number of Blaze workers per command

Each step is `blaze run` under `env -i`-like conditions (ran.Command: the whole environment is
HOME, PATH, TERM, LANG and RUST_BACKTRACE, nothing inherited), held to exit 0 and to the
program's line (ran.py: no number without both). `cold` builds from nothing in a home of its own;
`noop` runs again with nothing changed; `edit` changes the body of `answer` and must print the
new value. `edition` is `blaze run -p smoke-edition`: the member on the blaze edition, which
names nothing of its standard library (the toolchain gives it bstd as `std`; a manifest line
naming bstd is the finding `toolchain-crate-named`, docs/v2/plan/design/toolchain.md "The
finding") and whose `main` names its one grant (`out: Out`, D104). It builds bstd's whole
closure, minutes of work. Beside each time it prints what blaze said it built. Each row's
stdout and stderr are kept under `<work>/logs/<step>.{stdout,stderr}`. Exits 1 when any row is
INVALID.

The workspace's Cargo.lock and blaze.lock are committed (the versions a run on 2026-11-12
resolved), so two rounds build the same crates. Without them the cold build writes both into
the checkout and `noop` then makes one unit (`cause="what its narrow key holds of the crate
smoke, Cargo.lock and blaze.lock changed"`, 5.9 s on int-1112 e5f0d2abe4): lane
firstedit-1112's lock-file feedback, open; its fix belongs in
`crates/blaze-cargo/src/serve/job.rs`, `ready` (the view must already hold the lock the build
writes). To see that cause, delete the two lock files from the copy.
"""

import hashlib
import json
import os
import platform
import re
import shutil
import subprocess
import sys
import typing
from dataclasses import dataclass
from pathlib import Path
from typing import Any, TypedDict

from typed_input import parse

# Run with `python3 -I`, which leaves the script's directory off the path: ran.py is beside it.
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import ran
from ran import Command, Proof

STEPS = ("cold", "noop", "edit", "edition")
# What the blaze-edition member prints (harness/v2/smoke/edition/src/main.rs).
EDITION = "smoke-edition: bstd built under blaze"
SOURCE = Path(os.path.dirname(os.path.abspath(__file__))) / "smoke"
# The repository the harness is run from: the module builder's `tree` (the shim, the target
# JSON, pins/toolchain).
TREE = SOURCE.parents[2]
# The host the rows run on (crates/blaze-cli/src/graph.rs `TRIPLE`): the row of pins/clang.json read.
HOST = "aarch64-apple-darwin"
# Every member of the C toolchain's shelf a native run starts or loads (blaze-build-script
# toolchain.rs `SHELF_MEMBERS`); a shelf unpacked without one is refused naming it.
CLANG_MEMBERS = (
    "bin/clang",
    "bin/clang++",
    "bin/clang-23",
    "bin/ld64.lld",
    "bin/lld",
    "bin/llvm-ar",
    "bin/llvm-nm",
    "lib/clang",
    "lib/libclang.dylib",
)


class Crate(TypedDict):
    """One crate the bootstrap builder is asked for: its name, version and the unit's features."""

    name: str
    version: str
    features: list[str]


class ClangPin(TypedDict):
    """The host's row of pins/clang.json: the archive's name and its SHA-256, which names its folder."""

    name: str
    sha256: str


class Program(TypedDict):
    """A program the pin table rows for the shelf (`shelf_programs`): its member in the archive and
    the member's own SHA-256."""

    member: str
    sha256: str


# The proc-macro crates of the workspace's lock blaze cannot compile yet (D52): each with the
# features the host side of the graph unifies to.
MACROS: tuple[Crate, ...] = ({"name": "serde_derive", "version": "1.0.229", "features": ["default"]},)
# The packages of the lock with a build script, each with its unit's exact feature set (the
# set is in the module's identity, and reaches the script again as `CARGO_FEATURE_*`).
SCRIPTS: tuple[Crate, ...] = (
    {"name": "crossbeam-deque", "version": "0.8.8", "features": ["default", "std"]},
    {"name": "crossbeam-epoch", "version": "0.9.21", "features": ["alloc", "std"]},
    {"name": "crossbeam-utils", "version": "0.8.23", "features": ["default", "std"]},
    {"name": "getrandom", "version": "0.3.4", "features": []},
    {"name": "libc", "version": "0.2.191", "features": ["default", "std"]},
    {"name": "parking_lot_core", "version": "0.9.12", "features": []},
    {"name": "serde", "version": "1.0.229", "features": ["default", "derive", "serde_derive", "std"]},
    {"name": "serde_core", "version": "1.0.229", "features": ["result", "std"]},
    {"name": "serde_json", "version": "1.0.154", "features": ["default", "std"]},
    {"name": "smoke-sys", "version": "0.1.0", "features": []},
    {"name": "zmij", "version": "1.0.23", "features": []},
)


@dataclass(frozen=True)
class Input:
    blaze: str
    work: str
    tmpfs: str
    modules: str = ""
    toolchain_home: str = ""
    steps: tuple[str, ...] = STEPS
    path: str = "/run/current-system/sw/bin:/usr/local/bin:/usr/bin:/bin"
    shelf: str | None = None
    clang_shelf: str | None = None
    workers: int | None = None


def filesystem(path: Path) -> str:
    """The type of the file system `path` is on by /proc/mounts (the longest mount point that
    holds it), or "" where there is no /proc/mounts to ask (macOS)."""
    try:
        mounts = Path("/proc/mounts").read_text().splitlines()
    except OSError:
        return ""
    at = os.path.realpath(path)
    best = ("", "")
    for line in mounts:
        parts = line.split()
        if len(parts) < 3:
            continue
        point = parts[1].replace("\\040", " ")
        if (at == point or at.startswith(point.rstrip("/") + "/")) and len(point) > len(best[0]):
            best = (point, parts[2])
    return best[1]


def line(x: int) -> str:
    """What the program prints when `answer()` returns `x` (harness/v2/smoke/src/main.rs)."""
    return f'smoke {{"x":{x},"y":2}} CachePadded {{ value: Point {{ x: {x}, y: 2 }} }} 1'


def said(r: ran.Ran) -> str:
    """What blaze said it built and why, from its status line."""
    status = [it for it in (r.stdout + "\n" + r.stderr).splitlines() if it.startswith("blaze: ")]
    if not status:
        return "no status line"
    kept = re.findall(
        r'\b(?:built|cached|errors|unbuilt)=\d+|\b(?:cause|why|recompiled)=(?:"[^"]*"|\S+)', status[-1]
    )
    return " ".join(kept) or status[-1][:160]


def provision(asked: Input, project: Path, home: Path, logs: Path) -> bool:
    """D52: the proc-macro and build-script modules the workspace's lock names, built once by
    the bootstrap builder from the lock's own sources in its sandbox, and copied into `home`'s
    store. The builder runs under `toolchain_home` (the pinned toolchain) and `path` (cc), and
    nothing else of the caller's environment. Whether every module was built."""
    builder = Path(asked.modules)
    staging = logs.parent / "modules"
    shutil.rmtree(staging, ignore_errors=True)
    staging.mkdir(parents=True)
    store = staging / "store"
    environment = {"HOME": asked.toolchain_home, "PATH": asked.path, "RUST_BACKTRACE": "0"}
    good = True
    for kind, crates in (("proc-macro", MACROS), ("build-script", SCRIPTS)):
        work = staging / kind / "work"
        request = {
            "checkout": str(project),
            "store": str(store),
            "work": str(work),
            "tree": str(TREE),
            "crates": list(crates),
            "unsandboxed_build_scripts": False,
            "build_seconds": 900,
        }
        payload = {"build_scripts": request} if kind == "build-script" else request
        log = logs / f"modules-{kind}.log"
        with log.open("w") as held:
            code = subprocess.run(
                [str(builder), json.dumps(payload)],
                cwd=TREE,
                env=environment,
                stdin=subprocess.DEVNULL,
                stdout=held,
                stderr=subprocess.STDOUT,
                timeout=3600,
                check=False,
            ).returncode
        text = log.read_text()
        if code != 0:
            print(f"modules {kind:12s} INVALID: `{builder.name}` exited {code}: {ran.tail(text)}", flush=True)
            good = False
            continue
        made = sorted((store / kind / "modules").glob("*/module.wasm"))
        if len(made) != len(crates):
            print(
                f"modules {kind:12s} INVALID: {len(made)} module(s) under {store / kind / 'modules'}, not the {len(crates)} asked for",
                flush=True,
            )
            good = False
            continue
        print(
            f"modules {kind:12s} {len(made)} module(s) built by `{builder.name}` from {project / 'Cargo.lock'}; sandbox required; log {log}",
            flush=True,
        )
    if good:
        destination = home / ".blaze/v2"
        for kind in ("proc-macro", "build-script"):
            records = store / kind / "modules"
            for path in sorted(records.rglob("*")):
                if path.is_symlink():
                    sys.exit(f"the module builder made a symlink: {path}")
                target = destination / kind / "modules" / path.relative_to(records)
                if path.is_dir():
                    target.mkdir(parents=True, exist_ok=True)
                else:
                    target.parent.mkdir(parents=True, exist_ok=True)
                    shutil.copyfile(path, target)
    return good


def clang_shelf(asked: Input) -> Path:
    """The pinned C toolchain's folder under the given `clang_shelf` (the default: the user's),
    checked against pins/clang.json before any row runs: the folder is named by the host's
    archive digest, holds every member of `CLANG_MEMBERS`, and each program the pin table rows
    (`shelf_programs`) has that row's SHA-256. A refusal names what is missing or differs."""
    if (platform.system(), platform.machine()) != ("Darwin", "arm64"):
        raise ValueError("the smoke's C rows require the macOS ARM64 pinned native C backend")
    pin = typing.cast(dict[str, Any], json.loads((TREE / "pins/clang.json").read_text()))
    row = typing.cast(ClangPin, typing.cast(dict[str, Any], pin["targets"])[HOST])
    root = (
        Path(asked.clang_shelf)
        if asked.clang_shelf is not None
        else Path.home() / ".blaze/v2/toolchain/clang"
    )
    shelf = root / row["sha256"]
    if not shelf.is_dir():
        raise ValueError(
            f"clang_shelf {root} holds no `{row['sha256']}`: the pinned C toolchain ({row['name']}) is not unpacked there"
        )
    for member in CLANG_MEMBERS:
        if not (shelf / member).exists():
            raise ValueError(
                f"clang_shelf {shelf} holds no `{member}`: the pinned C toolchain was unpacked without it"
            )
    for name, value in typing.cast(dict[str, Any], pin["shelf_programs"])[HOST].items():
        if not isinstance(value, dict):
            continue  # `names`: the sorted row names, not a program.
        program = typing.cast(Program, value)
        path = shelf / program["member"].split("/", 1)[1]
        with path.open("rb") as held:
            digest = hashlib.file_digest(held, "sha256").hexdigest()
        if digest != program["sha256"]:
            raise ValueError(
                f"clang_shelf {path} is not `{name}`: sha256 {digest}, the pin rows {program['sha256']}"
            )
    return shelf


def place(name: str, root: Path, asked: Input, clang: Path | None) -> bool:
    """The steps in a new copy and a new home under `root`; whether every row counted."""
    blaze = str(Path(asked.blaze) / "blaze")
    shutil.rmtree(root, ignore_errors=True)
    home = root / "home"
    home.mkdir(parents=True)
    shelf = Path(asked.shelf) if asked.shelf is not None else Path.home() / ".blaze/v2/toolchain"
    toolchain = home / ".blaze/v2/toolchain"
    # The Rust shelf can contain the host's clang shelf. Copy only the pinned C folder
    # verified for this run, including when it came from the same shelf.
    shutil.copytree(shelf, toolchain, ignore=lambda directory, _: {"clang"} if Path(directory) == shelf else set())
    if clang is not None:
        shutil.copytree(clang, toolchain / "clang" / clang.name, symlinks=True)
    project = root / "p"
    shutil.copytree(SOURCE, project)
    env = {"HOME": str(home), "PATH": asked.path, "TERM": "dumb", "LANG": "C.UTF-8", "RUST_BACKTRACE": "0"}
    workers = ("--workers", str(asked.workers)) if asked.workers is not None else ()
    logs = root / "logs"
    logs.mkdir()
    if asked.modules and not provision(asked, project, home, logs):
        return False
    good = True
    main = project / "src/main.rs"
    for step in asked.steps:
        x = 1
        if step == "edit":
            text = main.read_text()
            if text.count("\n    1\n") != 1:
                print(f"{name} edit: INVALID: src/main.rs has no one body `1` to edit", flush=True)
                good = False
                continue
            main.write_text(text.replace("\n    1\n", "\n    7\n"))
            x = 7
        if step == "edition":
            expected = EDITION
            r = ran.run(
                Command((blaze, "run", "-p", "smoke-edition", *workers), str(project), env, timeout=3600),
                Proof(stdout=expected),
            )
        else:
            expected = line(x)
            r = ran.run(
                Command((blaze, "run", "-p", "smoke", *workers), str(project), env, timeout=1800),
                Proof(stdout=expected),
            )
        (logs / f"{step}.stdout").write_text(r.stdout)
        (logs / f"{step}.stderr").write_text(r.stderr)
        invalid = r.invalid
        if not invalid and expected not in r.stdout.splitlines():
            invalid = f"stdout contains no complete line {expected!r}"
        good = good and not invalid
        text = (
            f"INVALID: {invalid}" if invalid else f"{r.ms:8.0f} ms  exit and the program's line checked"
        )
        print(f"{name + ' ' + step:16s} {text}  [{said(r)}]", flush=True)
    return good


def main(argv: list[str]) -> None:
    if len(argv) != 2:
        sys.exit("real_smoke.py takes one JSON value (see its module docs)")
    asked = parse(Input, argv[1], "real_smoke.py")
    unknown = [it for it in asked.steps if it not in STEPS]
    if unknown or not asked.steps or list(asked.steps) != [it for it in STEPS if it in asked.steps]:
        sys.exit(f"steps is some of {STEPS} in that order, not {asked.steps}")
    if "edit" in asked.steps and "cold" not in asked.steps:
        sys.exit("the `edit` step needs `cold` before it: it edits the sources the cold build built")
    if asked.workers is not None and asked.workers < 1:
        sys.exit("workers must be positive")
    for name in ("blaze", "blaze-launcher", "blaze-fact-shim"):
        if not (Path(asked.blaze) / name).is_file():
            sys.exit(f"{asked.blaze} holds no `{name}`: give the directory of an installed blaze")
    if asked.modules and not (os.access(asked.modules, os.X_OK) and Path(asked.modules).is_file()):
        sys.exit(
            f"modules {asked.modules} is not an executable file: give the bootstrap module builder (scripts/src/bin/proc_macro_modules.rs)"
        )
    if asked.modules and not asked.toolchain_home:
        sys.exit(
            "toolchain_home is needed with modules: the home whose .rustup/toolchains holds the pinned toolchain"
        )
    if asked.toolchain_home and not (Path(asked.toolchain_home) / ".rustup" / "toolchains").is_dir():
        sys.exit(f"toolchain_home {asked.toolchain_home} holds no .rustup/toolchains")
    disk = filesystem(Path(asked.work).parent if not Path(asked.work).exists() else Path(asked.work))
    if disk == "tmpfs":
        sys.exit(
            f"work {asked.work} is on a tmpfs: the first run is on a disk (the second, `tmpfs`, is the tmpfs one)"
        )
    try:
        clang = clang_shelf(asked) if any(step != "edition" for step in asked.steps) else None
    except (OSError, ValueError, KeyError) as e:
        sys.exit(f"real_smoke.py: {e}")
    print(f"real smoke of {asked.blaze}, load {os.getloadavg()[0]:.0f}", flush=True)
    good = place(f"disk({disk or '?'})", Path(asked.work), asked, clang)
    if asked.tmpfs:
        parent = Path(asked.tmpfs) if Path(asked.tmpfs).exists() else Path(asked.tmpfs).parent
        memory = filesystem(parent)
        if memory != "tmpfs":
            sys.exit(
                f"tmpfs {asked.tmpfs} is on {memory or 'a file system /proc/mounts does not name'}, not a tmpfs"
            )
        good = place("tmpfs", Path(asked.tmpfs), asked, clang) and good
        shutil.rmtree(asked.tmpfs, ignore_errors=True)
    else:
        print("tmpfs            NOT RUN: no `tmpfs` directory was given", flush=True)
    sys.exit(0 if good else 1)


if __name__ == "__main__":
    main(sys.argv)
