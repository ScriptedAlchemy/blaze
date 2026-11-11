#!/usr/bin/env python3
"""rival.py: selected Blaze builds, ours and Astral's fork, on one machine.

    rival.py '{"arms": [...], "projects": [...], "work": "/fresh/reflink/work", "out": "/new/report"}'
    rival.py '{"action": "report", "out": "/existing/report"}'

Astral (astral-experiments/blaze) branched from our main at 6b3ed181 and tunes its fork for
uv, ruff and ty. This is the score: every project, theirs and ours, built by each arm from the
same pinned source, each arm in a checkout, home, store and target directory of its own.

Per project and arm:
  cold       the first build from nothing (no target, empty store/std, no daemon)
  no-op      the build again with nothing changed
  body       a statement added to one function's body (bench.py's `body` edit)
  signature  a method gets a parameter and every caller the argument (bench.py's `signature`)
  tests      the project's tests (`test` of its spec), twice; the second is timed;
             every test has to pass by its own verdict but those `test_known` names
  output     after the cold build and the first of each edit the program runs (`run` of its
             spec); each edit and reversal must say what that arm's cold build said
Each phase but cold runs `runs` times. The arms take turns in a rotating order (round r starts
with arm r mod n), never at once, so a slow minute of the machine falls on every arm alike; an
edit is made in an arm's checkout, built, taken back and built again before the next arm's turn.
Every accepted build identifies the selected daemon in its typed build-finished message.
An unsupported interface or another producer is a retained failure, never a timing or a
replacement compiler. A help-only probe refuses the old Astral CLI before its Cargo dispatcher
can run. No Rustup/Cargo setup, standard-library substitute or warm-up fallback runs here.

Input, one JSON object:
  arms      [{"name", "kind": "blaze"|"astral", "bin", "commit"}]: `bin` is an absolute
            directory with that arm's `blaze` and its companion executables; `commit` names
            the source built. Every executable is hashed before and after the whole run.
  projects  names from PROJECTS below (uv, ruff, ty) and bench.py's PROJECTS
  work      a new directory outside a Rust workspace, on a filesystem proven to share clones;
            every arm has its own checkout, home, store, shared std and target under it
  out       where the results go: raw/<project>.json, host.json, commands.txt, README.md, and
            beside each project's JSON the output of each arm's first edit build and tests, with
            our blaze's profile of that edit (`BLAZE_PROFILE`; ui.perfetto.dev opens its trace.json)
  runs      timed builds per phase (default 5)
  tests     run each project's tests (default true)
  interleave  the arms take turns within each phase (default true); false measures each
            arm on its own, all its phases, and stops its daemon before the next,
            for a machine that cannot hold two daemons with the project loaded (16 GB and ruff)
  machine   what the machine is, for host.json and the README (for example "ix arm64 16 vCPU")
  action    "run" (default), or "report" with only out to read existing results

Every command run is appended to commands.txt with its arm, directory and the variables it
was given beyond the machine's own.
"""
import contextlib, json, os, platform, re, shutil, statistics, subprocess, sys, time, typing
from dataclasses import MISSING, asdict, dataclass, fields, replace
from pathlib import Path

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path[:0] = [HERE, os.path.join(HERE, "v2")]
import bench, benchschema, blaze_cli, clone_out, storelib
from blaze_cli import Side, difference, program_output, test_results, test_verdict

LET = bench.LET
# Astral's projects, at the revisions its downstream workflow pins (downstream-tests.yaml).
PROJECTS = {
    "uv": dict(
        repo="astral-sh/uv", tag="5b5a53e1", rev="5b5a53e1803d426ba04f4af8ff7d6ff747548218",
        build=["-p", "uv", "--bin", "uv"],
        # uv's own lock file of its Python tooling, read and printed with no network.
        run=["tree", "--frozen", "--offline", "--no-config"],
        edits=dict(
            body=bench.body("crates/uv/src/lib.rs",
                            "pub async fn run(cli: Cli, global_initialization: GlobalInitialization) -> Result<ExitStatus> {"),
            signature=bench.parameter("crates/uv-python-interpreter/src/interpreter.rs", "sys_executable", "&Path", "crates/**/*.rs"),
        ),
        test=["-p", "uv-pep440", "-p", "uv-pep508"],
    ),
    "ruff": dict(
        repo="astral-sh/ruff", tag="07872adb", rev="07872adbafa7c00f42f1174d8aa3c3e324033009",
        build=["-p", "ruff", "--bin", "ruff"],
        run=["check", "--no-cache", "--isolated", "--select", "ALL", "--output-format", "concise",
             "crates/ruff_linter/resources/test/fixtures/pyflakes"],
        edits=dict(
            body=bench.body("crates/ruff/src/lib.rs", "pub fn check(args: CheckCommand, global_options: GlobalConfigArgs) -> Result<ExitStatus> {"),
            signature=bench.parameter("crates/ruff_python_semantic/src/model.rs", "seen_typing", "bool", "crates/**/*.rs"),
        ),
        test=["-p", "ruff_python_parser", "-p", "ruff_python_ast"],
    ),
    "ty": dict(
        repo="astral-sh/ruff", tag="07872adb", rev="07872adbafa7c00f42f1174d8aa3c3e324033009",
        build=["-p", "ty", "--bin", "ty"],
        run=["check", "--no-progress", "--output-format", "concise", "--python-version", "3.12",
             "crates/ruff_linter/resources/test/fixtures/pyflakes"],
        edits=dict(
            body=bench.body("crates/ty/src/lib.rs", "fn run_check(args: CheckCommand) -> anyhow::Result<ExitStatus> {"),
            signature=bench.parameter("crates/ty_module_resolver/src/path.rs", "is_site_packages", "bool", "crates/**/*.rs"),
        ),
        test=["-p", "ty_module_resolver"],
    ),
}
EDITS = ("body", "signature")
KINDS = ("blaze", "astral")


@dataclass(frozen=True)
class Arm:
    name: str
    kind: str
    bin: str
    commit: str


@dataclass(frozen=True)
class Input:
    arms: list
    projects: list
    work: str
    out: str
    action: str = "run"
    runs: int = 5
    tests: bool = True
    interleave: bool = True
    machine: str = ""


@dataclass(frozen=True)
class Report:
    out: str
    action: str = "report"


def fail(message):
    sys.exit(f"rival.py: {message}")


def checked(cls, value, where):
    """`cls` from the dict `value`, every field of its exact type; exits naming the field."""
    if not isinstance(value, dict):
        fail(f"{where} is a JSON object, not {value!r}")
    known = {f.name for f in fields(cls)}
    if unknown := sorted(set(value) - known):
        fail(f"{where} has no field {', '.join(unknown)} (fields: {', '.join(sorted(known))})")
    hints = typing.get_type_hints(cls)
    for f in fields(cls):
        if f.name not in value:
            if f.default is MISSING and f.default_factory is MISSING:
                fail(f"{where} needs field {f.name}")
            continue
        if type(value[f.name]) is not hints[f.name]:
            fail(f"{where}.{f.name} is a {hints[f.name].__name__}, not {value[f.name]!r}")
    return cls(**value)


def read(text):
    if text == "-":
        text = sys.stdin.read()
    elif text and not text.lstrip().startswith("{"):
        text = open(text).read()
    try:
        return json.loads(text or "{}")
    except json.JSONDecodeError as e:
        fail(f"the input is not JSON: {e}")


def typed(text):
    given = checked(Input, read(text), "input")
    arms = [checked(Arm, arm, f"input.arms[{at}]") for at, arm in enumerate(given.arms)]
    for at, arm in enumerate(arms):
        if arm.kind not in KINDS:
            fail(f"input.arms[{at}].kind is one of {', '.join(KINDS)}, not {arm.kind!r}")
        if not re.fullmatch(r"[a-zA-Z0-9][a-zA-Z0-9_-]*", arm.name):
            fail(f"input.arms[{at}].name must name one directory, not {arm.name!r}")
        if not os.path.isabs(arm.bin) or not os.path.isdir(arm.bin):
            fail(f"input.arms[{at}].bin must name an absolute executable directory: {arm.bin!r}")
        if not re.fullmatch(r"[0-9a-f]{40}", arm.commit):
            fail(f"input.arms[{at}].commit must identify the source with a full Git commit")
    if len({arm.name for arm in arms}) != len(arms):
        fail("input.arms has two arms of one name")
    if {arm.kind for arm in arms} != set(KINDS) or sum(arm.kind == "blaze" for arm in arms) != 1:
        fail("input.arms needs one selected ours (blaze) and at least one selected Astral arm")
    if len({os.path.realpath(os.path.join(arm.bin, "blaze")) for arm in arms}) != len(arms):
        fail("input.arms must select different Blaze executables")
    for at, name in enumerate(given.projects):
        if type(name) is not str or name not in spec_of_all():
            fail(f"input.projects[{at}] is one of {', '.join(sorted(spec_of_all()))}, not {name!r}")
    if not given.projects or len(set(given.projects)) != len(given.projects):
        fail("input.projects must be a nonempty list without duplicates")
    if given.runs < 1:
        fail(f"input.runs is at least 1, not {given.runs}")
    if given.action != "run":
        fail("input.action must be run or report")
    if not given.work or not given.out:
        fail("input.work and input.out must name new directories")
    return given, arms


def spec_of_all():
    return {**bench.PROJECTS, **PROJECTS}


class Log:
    """commands.txt: every command, its arm, where it ran and what it was given."""

    def __init__(self, path):
        self.path = path

    def ran(self, arm, ws, env, argv):
        extra = {k: v for k, v in env.items() if os.environ.get(k) != v and k != "PATH"}
        with open(self.path, "a") as out:
            out.write(f"[{arm}] cd {ws} && {' '.join(f'{k}={v}' for k, v in sorted(extra.items()))} {' '.join(argv)}\n")


class Runner:
    """One arm on one project: its checkout, its environment, how to build and what it said."""

    def __init__(self, arm, ws, work, project, spec, log):
        self.arm, self.ws, self.log = arm, ws, log
        self.home = os.path.join(work, f"home-{arm.name}")
        env = {k: v for k, v in os.environ.items() if not k.startswith(("BLAZE_", "RA_JIT_"))}
        env.update(CARGO_TERM_COLOR="never", RUST_BACKTRACE="0", INSTA_UPDATE="no",
                   CARGO_UNSTABLE_BUILD_DIR_NEW_LAYOUT="false", **spec.get("env", {}))
        for name in ("RUSTFLAGS", "CARGO_TARGET_DIR", "RUSTC_WORKSPACE_WRAPPER", "CARGO_BUILD_TARGET",
                     "RUSTC_WRAPPER", "RUSTUP_TOOLCHAIN", "RUSTC", "RUSTDOC", "CARGO",
                     "CARGO_ENCODED_RUSTFLAGS", "RUSTDOCFLAGS", "CARGO_INCREMENTAL", "XDG_RUNTIME_DIR"):
            env.pop(name, None)
        env.update(BLAZE_HOME=self.home, BLAZE_CACHE=os.path.join(work, f"store-{arm.name}"),
                   XDG_CACHE_HOME=os.path.join(work, f"machine-{arm.name}"),
                   CARGO_TARGET_DIR=os.path.join(ws, "target"), BLAZE_LOCK="write", BLAZE_CORES="all")
        env[blaze_cli.COMPILER_ENV] = os.path.realpath(os.path.join(arm.bin, "blaze"))
        self.side = Side(ws, env, True)
        self.env = self.side.env

    def run(self, command, *args, env=None):
        self.log.ran(self.arm.name, self.ws, dict(self.side.env, **(env or {})), self.side.argv(command, *args))
        return self.side.cargo(command, *args, env=env)

    def stop(self):
        _, ran = self.run("stop")
        if ran.returncode:
            raise RuntimeError(f"{self.arm.name}: stop failed: {ran.stderr[-1500:]}")

    def built(self, ran):
        """Only the typed producer verdict counts; a log marker is not compiler evidence."""
        return blaze_cli.how_built(ran)


def compiler_identity(arm):
    """Hash the selected launcher and every shipped executable, including the fork's ra-jit."""
    directory = Path(arm.bin).resolve(strict=True)
    program = directory / "blaze"
    if not program.is_file() or not os.access(program, os.X_OK):
        raise RuntimeError(f"{arm.name}: no executable selected Blaze at {program}")
    paths = sorted(p for p in directory.iterdir() if p.is_file() and os.access(p, os.X_OK))
    if any(path.resolve().parent != directory for path in paths):
        raise RuntimeError(f"{arm.name}: executables must reside in the selected directory, including companions")
    binaries = {str(path.resolve()): bench.sha256(path) for path in paths}
    selected = str(program.resolve())
    return dict(path=selected, sha256=binaries[selected], binaries=binaries)


def protocol(arm, work, out, log):
    """Refuse the old Cargo dispatcher with safe help before any toolchain/build/stop call."""
    runner = Runner(arm, work, work, "protocol", {}, log)
    identity = compiler_identity(arm)
    transcript = []

    def ask(argv, timeout=120):
        log.ran(arm.name, work, runner.env, argv)
        ran = subprocess.run(argv, cwd=work, env=runner.env, capture_output=True, text=True,
                             timeout=timeout, stdin=subprocess.DEVNULL)
        transcript.append(f"{argv!r}\nexit {ran.returncode}\n--- stdout\n{ran.stdout}\n--- stderr\n{ran.stderr}")
        if ran.returncode:
            raise RuntimeError(f"{arm.name}: unsupported compiler protocol: {argv[1:]} exited {ran.returncode}")
        return ran.stdout

    try:
        # Work has no ancestor Cargo.toml, so even a launcher without --ignore-pin cannot
        # select a workspace's other compiler. Astral's legacy --help never runs Cargo.
        help_text = ask([identity["path"], "--help"])
        for command in ("status", "toolchain"):
            if not re.search(rf"^\s*{command}\b[^\n]*--json", help_text, re.M):
                raise RuntimeError(f"{arm.name}: unsupported compiler protocol: --help does not advertise "
                                   f"{command} --json; refusing before toolchain setup or builds")
        chosen = storelib.blaze_toolchain_of(ask([identity["path"], "--ignore-pin", "toolchain", "--json"], timeout=900))
        if compiler_identity(arm) != identity:
            raise RuntimeError(f"{arm.name}: compiler identity changed during protocol validation")
        return dict(compiler=identity, toolchain=asdict(chosen))
    finally:
        Path(out, "raw", f"protocol-{arm.name}.txt").write_text("\n".join(transcript))


def fetch(name, spec, sources):
    """The pinned checkout, fetched at its revision when it is not there. Not bench.py's
    checkout: rival's uv, ruff and ty are pinned where Astral tests its fork, which bench.py's
    are not, and their `tag` is no ref to clone."""
    source = os.path.join(sources, spec["repo"].replace("/", "-") + "-" + spec["rev"][:12])
    if not os.path.isdir(source):
        os.makedirs(source + ".part", exist_ok=True)
        for argv in (["git", "init", "-q"], ["git", "fetch", "-q", "--depth", "1", f"https://github.com/{spec['repo']}", spec["rev"]],
                     ["git", "checkout", "-q", "FETCH_HEAD"]):
            subprocess.run(argv, cwd=source + ".part", check=True)
        os.rename(source + ".part", source)
    head = subprocess.run(["git", "rev-parse", "HEAD"], cwd=source, capture_output=True, text=True).stdout.strip()
    if head != spec["rev"]:
        raise RuntimeError(f"{name} is at {head}, not at the {spec['rev']} it is pinned to")
    return source


def turns(arms, round_):
    return arms[round_ % len(arms):] + arms[:round_ % len(arms)]


def median(values):
    return round(statistics.median(values), 3) if values else None


def measure(name, spec, arms, given, log):
    work = os.path.realpath(os.path.join(given.work, name, arms[0].name))
    os.makedirs(work)
    source = fetch(name, spec, os.path.join(given.work, "sources"))
    runners = {}
    required = ["cold", "no-op", *(kind for kind in EDITS if spec["edits"].get(kind))]
    if given.tests and spec.get("test") is not None:
        required += ["tests_first", "tests"]
    result = dict(project=name, repo=spec["repo"], rev=spec["rev"], required=required,
                  arms={a.name: dict(kind=a.kind, phases={}, wrong=[], failed=None) for a in arms})
    try:
        for arm in arms:
            ws = os.path.join(work, arm.name)
            shutil.copytree(source, ws, symlinks=True, ignore=shutil.ignore_patterns("target", "target-ref", ".git"))
            runners[arm.name] = Runner(arm, ws, work, name, spec, log)
        measure_turns(name, spec, arms, given, runners, result)
    except (RuntimeError, subprocess.SubprocessError, OSError, AssertionError, ValueError, storelib.Failed) as error:
        for arm in arms:
            row = result["arms"][arm.name]
            row["failed"] = row["failed"] or str(error)[-2000:]
    finally:
        for arm in arms:
            if arm.name not in runners:
                continue
            try:
                runners[arm.name].stop()
            except (RuntimeError, subprocess.SubprocessError, OSError, ValueError, storelib.Failed) as error:
                row = result["arms"][arm.name]
                row["failed"] = "; ".join(filter(None, (row["failed"], f"cleanup: {error}")))
    return result


def measure_turns(name, spec, arms, given, runners, result):
    """One schedule of actual selected arms; a refused producer never supplies a sample."""
    build, run = spec.get("build", []), spec.get("run")
    expected = {}

    def row(arm):
        return result["arms"][arm.name]

    def live(arm):
        return row(arm)["failed"] is None

    def output_is_right(arm, label):
        if run is None or not live(arm):
            return
        _, ran = runners[arm.name].run("run", *build, "--", *run)
        if runners[arm.name].built(ran) != "daemon":
            row(arm)["failed"] = f"{label} output was not built by the selected daemon: {runners[arm.name].built(ran)}"
            return
        said = program_output(ran)
        if arm.name not in expected:
            expected[arm.name] = said
        elif wrong := difference(said, expected[arm.name]):
            row(arm)["wrong"].append(f"{label}: {wrong}"[:400])

    def accepted(arm, label, ran):
        if ran.returncode != 0:
            row(arm)["failed"] = f"{label} build exited {ran.returncode}: {(ran.stderr or ran.stdout)[-1500:]}"
            return False
        built = runners[arm.name].built(ran)
        if built != "daemon":
            row(arm)["failed"] = f"{label} build was not produced by the selected daemon: {built}"
            return False
        return True

    def timed_build(arm, label):
        if not live(arm):
            return None
        took, ran = runners[arm.name].run("build", *build)
        return (round(took, 3), "daemon") if accepted(arm, label, ran) else None

    print(f"{name}: cold", flush=True)
    for arm in turns(arms, len(name)):
        if not live(arm):
            continue
        if got := timed_build(arm, "cold"):
            took, built = got
            row(arm)["phases"]["cold"] = dict(build_s=took, ready_s=0.0, wall_s=took,
                                              samples_s=[took], built=built)
            print(f"  {arm.name:10} cold {took:8.2f}s  {built}", flush=True)
    for arm in arms:
        output_is_right(arm, "cold")

    def phase(label, samples):
        for arm in arms:
            got = samples[arm.name]
            if got:
                row(arm)["phases"][label] = dict(wall_s=median([s[0] for s in got]), samples_s=[s[0] for s in got],
                                                 built=sorted({s[1] for s in got}), **({"back_s": median([s[2] for s in got])} if len(got[0]) > 2 else {}))
        print(f"  {label:10} " + "  ".join(f"{a.name} {row(a)['phases'][label]['wall_s']:.3f}s" for a in arms if label in row(a)["phases"]), flush=True)

    print(f"{name}: no-op", flush=True)
    samples = {a.name: [] for a in arms}
    for round_ in range(given.runs):
        for arm in turns(arms, round_):
            if live(arm) and (got := timed_build(arm, "no-op")):
                samples[arm.name].append(got)
    phase("no-op", samples)

    for kind in EDITS:
        changes = spec["edits"].get(kind)
        if not changes:
            continue
        samples = {a.name: [] for a in arms}
        rejected = {}
        for round_ in range(given.runs):
            for arm in turns(arms, round_):
                if not live(arm) or arm.name in rejected:
                    continue
                r = runners[arm.name]
                bench.write_edit(r.ws, changes, False)
                trace = os.path.realpath(os.path.join(given.out, "raw", f"{name}-{arm.name}-{kind}.profile"))
                took, ran = r.run("build", *build, env={"BLAZE_PROFILE": trace} if round_ == 0 and arm.kind == "blaze" else None)
                if round_ == 0:
                    with open(os.path.join(given.out, "raw", f"{name}-{arm.name}-{kind}.txt"), "w") as f:
                        f.write(f"took {took:.3f}s, exit {ran.returncode}\n--- stdout\n{ran.stdout[-20000:]}\n--- stderr\n{ran.stderr[-40000:]}")
                if not accepted(arm, kind, ran):
                    rejected[arm.name] = row(arm)["failed"]
                    print(f"    {arm.name}: {kind} edit rejected\n" + "".join(f"      {l}\n" for l in rejected[arm.name].strip().splitlines()[-8:]), flush=True)
                    made = None
                else:
                    made = round(took, 3), "daemon"
                if made and round_ == 0:
                    output_is_right(arm, kind)
                bench.write_edit(r.ws, changes, True)
                back = timed_build(arm, f"{kind} reversed")
                if back and round_ == 0:
                    output_is_right(arm, f"{kind} reversed")
                if made and back:
                    samples[arm.name].append((made[0], made[1], back[0]))
        phase(kind, samples)
        for arm in arms:
            if arm.name in rejected:
                row(arm)["wrong"].append(f"{kind}: rejects the edit or the required compiler producer"[:400])
                row(arm)["phases"][kind] = dict(rejected=rejected[arm.name][-400:])

    if given.tests and spec.get("test") is not None:
        args = spec["test"]
        print(f"{name}: tests", flush=True)
        outcomes = {}
        known = frozenset(spec.get("test_known", ()))
        for round_ in range(2):
            for arm in turns(arms, round_):
                if not live(arm):
                    continue
                took, ran = runners[arm.name].run("test", *args)
                outcomes[arm.name] = (round(took, 3), ran)
                producer = runners[arm.name].built(ran)
                if producer != "daemon":
                    row(arm)["failed"] = f"tests round {round_ + 1}: not the selected daemon: {producer}"
                status, tests, totals = test_results(ran)
                wrong = test_verdict((status, tests, totals), known)
                if wrong:
                    row(arm)["wrong"].append(f"tests round {round_ + 1}: {wrong}"[:400])
                entry = dict(wall_s=round(took, 3), built=producer, status=status,
                             passed=totals[1], failed=totals[2], ignored=totals[3],
                             tests=len({test for test, _ in tests}), same=wrong is None, difference=wrong)
                row(arm)["phases"]["tests_first" if round_ == 0 else "tests"] = entry
        for arm in arms:
            if arm.name not in outcomes:
                continue
            took, ran = outcomes[arm.name]
            with open(os.path.join(given.out, "raw", f"{name}-{arm.name}-tests.txt"), "w") as f:
                f.write(f"kept: {getattr(ran, 'kept', [])}\n--- stdout\n{ran.stdout}\n--- stderr\n{ran.stderr[-20000:]}")
            entry = row(arm)["phases"].get("tests", row(arm)["phases"]["tests_first"])
            print(f"  {arm.name:10} tests {took:8.2f}s  {entry['passed']} passed {entry['failed']} failed  {entry['difference'] or 'all pass'}", flush=True)


def host(given, arms):
    mem = open("/proc/meminfo").read().split("\n")[0] if os.path.exists("/proc/meminfo") else None
    return dict(schema=2, machine=given.machine, platform=platform.platform(), cpus=os.cpu_count(),
                affinity=len(os.sched_getaffinity(0)) if hasattr(os, "sched_getaffinity") else None,
                memory=mem, date=time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
                arms=[dict(name=a.name, kind=a.kind, commit=a.commit) for a in arms],
                projects=given.projects, runs=given.runs, tests=given.tests, interleave=given.interleave, errors=[])


PHASES = ("cold", "no-op", "body", "signature", "tests")


def evidence_errors(info, rows):
    """Current comparisons require every requested arm/phase; historical labels stay intact."""
    errors = list(info.get("errors", []))
    if not rows:
        errors.append("no projects were measured")
    native = info.get("schema") == 2
    if "schema" in info and not native:
        errors.append(f"unsupported rival schema {info['schema']!r}")
    if native:
        if {row.get("project") for row in rows} != set(info["projects"]):
            errors.append("the requested projects were not all measured")
        for arm in info["arms"]:
            if not arm.get("compiler") or arm.get("compiler_after") != arm["compiler"]:
                errors.append(f"{arm['name']}: compiler identity was not verified unchanged after cleanup")
            if not arm.get("toolchain"):
                errors.append(f"{arm['name']}: required compiler protocol was not established")
    for row in rows:
        if row.get("error"):
            errors.append(f"{row['project']}: {row['error']}")
        for selected in info["arms"]:
            name = selected["name"]
            arm = row.get("arms", {}).get(name)
            if not isinstance(arm, dict):
                errors.append(f"{row['project']}/{name}: arm was not measured")
                continue
            prefix = f"{row['project']}/{name}"
            if arm.get("failed"):
                errors.append(f"{prefix}: {arm['failed']}")
            errors += [f"{prefix}: {why}" for why in arm.get("wrong", [])]
            if not native:
                continue
            spec = spec_of_all()[row["project"]]
            required = ["cold", "no-op", *(kind for kind in EDITS if spec["edits"].get(kind))]
            if info["tests"] and spec.get("test") is not None:
                required += ["tests_first", "tests"]
            for label in required:
                phase = arm.get("phases", {}).get(label)
                if not isinstance(phase, dict) or not benchschema.number(phase.get("wall_s")):
                    errors.append(f"{prefix} {label}: no valid timing")
                    continue
                if phase.get("built") not in ("daemon", ["daemon"]):
                    errors.append(f"{prefix} {label}: not built by the selected daemon")
                if label not in ("tests_first", "tests"):
                    samples = phase.get("samples_s")
                    count = 1 if label == "cold" else info["runs"]
                    if not isinstance(samples, list) or len(samples) != count or not all(map(benchschema.number, samples)):
                        errors.append(f"{prefix} {label}: incomplete timing samples")
                if label in EDITS and not benchschema.number(phase.get("back_s")):
                    errors.append(f"{prefix} {label}: no valid reversal time")
                if label in ("tests_first", "tests") and (
                    phase.get("same") is not True or phase.get("difference") is not None
                    or type(phase.get("tests")) is not int or phase["tests"] < 1
                ):
                    errors.append(f"{prefix} {label}: no complete passing test verdict")
    return errors


def report(out):
    """README.md from raw/*.json and host.json: one row per project and phase, each arm's median,
    and the gap of our blaze to the best other arm (above 1.00x: someone beats us)."""
    info = json.load(open(os.path.join(out, "host.json")))
    rows = [json.load(open(os.path.join(out, "raw", f))) for f in sorted(os.listdir(os.path.join(out, "raw"))) if f.endswith(".json")]
    names = [a["name"] for a in info["arms"]]
    ours = next(a["name"] for a in info["arms"] if a["kind"] == "blaze")
    errors = evidence_errors(info, rows)
    historical = any(a["kind"] == "cargo" for a in info["arms"])
    title = "our Blaze, Astral's fork and Cargo (historical)" if historical else "our Blaze and Astral's fork"
    cold = "first build + the recorded daemon wait" if historical else "first build from empty state"
    lines = [f"# Rival score: {title} ({info['date'][:10]})", "",
             f"Machine: {info['machine'] or info['platform']}, {info['affinity'] or info['cpus']} cores used. "
             f"{info['runs']} timed builds per phase, "
             f"{'arms in turn' if info.get('interleave', True) else 'one daemon at a time'} (harness/rival.py). Commits: "
             + ", ".join(f"{a['name']} ({a['kind']}) `{a['commit'][:10]}`" for a in info["arms"]) + ".", "",
             f"Seconds, median. cold = {cold}. tests = the second `test` run. "
             f"gap = {ours} / fastest other arm: above 1.00x someone beats us. A cell with `!` was built by cargo "
             "in the historical record; `x` failed; `rejected`: the edit did not build.", "",
             ("Comparison refused; no ratios are reported." if errors else
              "Historical measurements; the original producer labels are preserved." if historical else
              "Every requested measurement passed its verdict."), "",
             "| project | phase | " + " | ".join(names) + " | gap |", "|---|---|" + "---:|" * (len(names) + 1)]
    for r in rows:
        for label in PHASES:
            cells, times = [], {}
            for n in names:
                arm = r["arms"].get(n, {})
                p = arm.get("phases", {}).get(label)
                if p is None:
                    cells.append("x" if arm.get("failed") else "")
                    continue
                if not isinstance(p, dict):
                    cells.append("invalid")
                    continue
                if "rejected" in p:
                    cells.append("rejected")
                    continue
                if not benchschema.number(p.get("wall_s")):
                    cells.append("invalid")
                    continue
                if not errors:
                    times[n] = p["wall_s"]
                built = p.get("built")
                flag = "!" if (isinstance(built, list) and any(isinstance(b, str) and b.startswith("cargo:") for b in built)) or (isinstance(built, str) and built.startswith("cargo:")) else ""
                extra = f" ({p.get('passed', '?')} ok, {p.get('failed', '?')} failed)" if label == "tests" else ""
                cells.append(f"{p['wall_s']:.2f}{flag}{extra}")
            if not any(cells):
                continue
            others = [t for n, t in times.items() if n != ours]
            gap = f"{times[ours] / min(others):.2f}x" if ours in times and others and min(others) > 0 else ""
            lines.append(f"| {r['project']} | {label} | " + " | ".join(cells) + f" | {gap} |")
    wrong = [(r["project"], n, w) for r in rows for n, a in r["arms"].items() for w in a.get("wrong", [])]
    failed = [(r["project"], n, a["failed"]) for r in rows for n, a in r["arms"].items() if a.get("failed")]
    lines += ["", "## Wrong output or tests", ""]
    lines += [f"- {p} / {n}: {w}" for p, n, w in wrong] or ["None."]
    lines += ["", "## Failed", ""]
    lines += [f"- {p} / {n}: {f.splitlines()[0][:200] if f else ''}" for p, n, f in failed] or ["None."]
    lines += ["", "## Evidence refusals", ""]
    lines += [f"- {error}" for error in errors] or ["None."]
    lines += ["", "Raw results: raw/<project>.json. Every command: commands.txt. Machine: host.json.", ""]
    open(os.path.join(out, "README.md"), "w").write("\n".join(lines))
    print("\n".join(lines))
    return errors


def main():
    argv = sys.argv[1:]
    if len(argv) != 1:
        fail(f"usage: rival.py ['<json>' | FILE | -]\n\n{__doc__}")
    value = read(argv[0])
    if isinstance(value, dict) and value.get("action") == "report":
        sys.exit(1 if report(checked(Report, value, "input").out) else 0)
    given, arms = typed(json.dumps(value))
    given = replace(given, work=os.path.realpath(given.work), out=os.path.realpath(given.out))
    if os.path.exists(given.out):
        fail(f"out already exists; refusing to mix evidence: {given.out}")
    os.makedirs(os.path.join(given.out, "raw"))
    info = host(given, arms)
    host_path = Path(given.out, "host.json")
    host_path.write_text(json.dumps(info, indent=1))
    log = Log(os.path.join(given.out, "commands.txt"))
    snapshots = {}
    try:
        if os.path.exists(given.work):
            raise RuntimeError(f"work already exists; refusing to reuse compiler state: {given.work}")
        if any((parent / "Cargo.toml").exists() for parent in Path(given.work).parents):
            raise RuntimeError("work must be outside a Rust workspace so the help probe cannot follow a compiler pin")
        os.makedirs(given.work)
        with open(os.path.join(given.out, "reflink.txt"), "w") as evidence, contextlib.redirect_stdout(evidence):
            clone_out.selftest(given.work)
        for arm, record in zip(arms, info["arms"]):
            try:
                snapshots[arm.name] = compiler_identity(arm)
                record["compiler"] = snapshots[arm.name]
                record.update(protocol(arm, given.work, given.out, log))
            except (RuntimeError, subprocess.SubprocessError, OSError, ValueError, storelib.Failed) as error:
                info["errors"].append(f"{arm.name}: {error}")
    except (RuntimeError, OSError, AssertionError, clone_out.engine.Failed) as error:
        info["errors"].append(f"prerequisite: {error}")
    host_path.write_text(json.dumps(info, indent=1))
    specs = spec_of_all()
    blocks = [arms] if given.interleave else [[arm] for arm in arms]
    for name in given.projects:
        result = dict(project=name, repo=specs[name]["repo"], rev=specs[name]["rev"], arms={})
        if info["errors"]:
            result["arms"] = {a.name: dict(kind=a.kind, phases={}, wrong=[],
                                           failed="not measured: compiler protocol or placement prerequisite failed") for a in arms}
        try:
            for block in ([] if info["errors"] else blocks):
                got = measure(name, specs[name], block, given, log)
                result["arms"].update(got["arms"])
        except (RuntimeError, subprocess.SubprocessError, OSError, AssertionError, ValueError, storelib.Failed) as e:
            result["error"] = str(e)[-2000:]
            print(f"{name}: {e}", flush=True)
        json.dump(result, open(os.path.join(given.out, "raw", f"{name}.json"), "w"), indent=1)
    for arm, record in zip(arms, info["arms"]):
        if arm.name not in snapshots:
            continue
        try:
            record["compiler_after"] = compiler_identity(arm)
            if record["compiler_after"] != snapshots[arm.name]:
                info["errors"].append(f"{arm.name}: compiler identity changed before cleanup completed")
        except (RuntimeError, OSError) as error:
            info["errors"].append(f"{arm.name}: compiler identity after cleanup: {error}")
    host_path.write_text(json.dumps(info, indent=1))
    sys.exit(1 if report(given.out) else 0)


if __name__ == "__main__":
    main()
