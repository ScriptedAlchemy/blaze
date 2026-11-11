#!/usr/bin/env python3
"""smoke.py: small programs and real projects that must build and run correctly with `blaze run`
and `blaze test`, in dev and --release (docs/release.md, "The smoke corpus").

usage: smoke.py [INPUT]          one JSON object; command is run (default), apt or brew

INPUT is one JSON object (inline, a file's path, or `-` for stdin; none is `{}`), every field
optional:

    {"command": "run", "groups": ["std", "crates", "projects", "apps"], "only": [NAME, ...], "profiles": ["dev", "release"],
     "blaze": "path of the `blaze` to run (default: the one on PATH)", "work": DIR,
     "results": "a JSON file of every result", "blaze_rev": "the commit of the project `blaze`",
     "corpus_rev": "the commit whose corpus the blaze is held to"}

smoke/smoke.json names the cases and projects. A case is a directory smoke/<name>: a cargo
project whose `blaze run` prints exactly smoke/<name>/expected.txt (given `args` and `env`) and
whose `blaze test` passes `tests` tests. A project is a repository at a pinned commit (`rev`;
`{blaze}` is `blaze_rev`, by default this checkout's HEAD), whose `runs` print what they say
(`stdout` exactly, or `contains`, or a `file` that `starts` so, each character of it a byte)
and whose `blaze test` (with the arguments of `test`) passes. A private repository is fetched with GH_TOKEN. A project may have
`env` for its builds, `seconds` for each command (default 3600), and `prepare`, a module of the
harness whose `prepare(checkout)` gives the tree to build and the environment its builds need
(bun: bunprep.py, its own build of its C and C++; flox: floxprep.py, its Nix build's; servo:
servoprep.py, the sandbox opt-out its manifest needs), and
`fetches`, the selections (`-p`s, `--tests`) `blaze lock --fetches` records its build scripts'
downloads for first, one per feature set they pick it by (fframes'). A project's `dir` is where
in the checkout its workspace is (codex: `codex-rs`): its runs, tests and builds start there.
Its `builds` are `blaze build` with `args` in that directory's `dir` (a workspace of its own, as
dynamo's Python bindings are), each then running `then` (`{target}` is that directory's
target/<profile>) to print what it says. `macos` holds the fields that are otherwise on macOS,
and `excluded` says what the corpus leaves out of a project and why (printed with its results),
so that the list shrinks.

Groups: `std` (no dependencies: the merge gate's, every pull request), `crates` (from
crates.io), `projects` (a project's default), and `apps` (the large applications, each in a job
of its own in promote.yaml, with bench.py's times beside cargo's); all four are the gate a canary
passes to become stable. Without `groups`, every group but `apps` runs.

With `corpus_rev` (promote.yaml: the release's commit), the manifest, project specifications,
case sources and expected outputs all come from that commit's harness/smoke/ tree. A case
introduced in this checkout after that manifest is reported as skipped; a selection with no
cases or projects to run is an error. The commit has to be in this checkout (a clone with its
history). Results name its exact commit, tree and snapshot digest. The gate's own code is this
checkout's. Every Blaze command uses the selected absolute executable with `--ignore-pin`.

A command that gives no answer in its time is followed by `blaze slow` in its directory, which
names what the build it was in spent the time on (AGENTS.md rule 16), and `blaze stop`, so a
daemon still at it holds neither the machine's memory nor its cores through what runs next.

A case or project with `open` (an issue) is run and reported, and its failure does not fail the
run. Each failure is printed with the command that repeats it. `results` is every result, for
promote.yaml's issues. Stdlib only.
"""
import base64, hashlib, importlib, json, os, shlex, shutil, stat, subprocess, sys, tarfile, tempfile, time, typing
from dataclasses import dataclass, fields, replace
from pathlib import Path, PurePosixPath

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(HERE)
CORPUS = os.path.join(HERE, "smoke")
# A project's `prepare` is a module beside this one.
sys.path.insert(0, HERE)
EXE = ".exe" if os.name == "nt" else ""
Group = typing.Literal["std", "crates", "projects", "apps"]
Profile = typing.Literal["dev", "release"]
GROUPS = ("std", "crates", "projects")
CASE_SECS = 900
PROJECT_SECS = 3600
CORPUS_BYTES = 64 * 1024 * 1024
CORPUS_FILES = 10_000
# What blaze prints when something of the build was not its own (install.yaml looks for these too).
NOT_BLAZES = ("blaze: bug:", "the daemon has no part")


@dataclass(frozen=True)
class Input:
    command: typing.Literal["run", "apt", "brew"] = "run"
    groups: tuple[Group, ...] = GROUPS
    only: tuple[str, ...] = ()
    profiles: tuple[Profile, ...] = ("dev", "release")
    blaze: str = ""
    work: str = ""
    results: str = ""
    blaze_rev: str = ""
    corpus_rev: str = ""


def typed(text):
    """`Input` from INPUT; exits naming the field that is wrong."""
    if text == "-":
        text = sys.stdin.read()
    elif text and not text.lstrip().startswith("{"):
        text = open(text).read()
    try:
        value = json.loads(text or "{}")
    except json.JSONDecodeError as e:
        sys.exit(f"smoke.py: INPUT is not JSON: {e}")
    if not isinstance(value, dict):
        sys.exit("smoke.py: INPUT is a JSON object")
    hints = typing.get_type_hints(Input)
    known = {f.name for f in fields(Input)}
    if unknown := sorted(set(value) - known):
        sys.exit(f"smoke.py: INPUT has no field {', '.join(unknown)} (fields: {', '.join(sorted(known))})")
    for name, given in value.items():
        hint = hints[name]
        if hint is str:
            ok = isinstance(given, str)
        elif typing.get_origin(hint) is typing.Literal:
            if not isinstance(given, str) or given not in typing.get_args(hint):
                sys.exit(f"smoke.py: INPUT field {name} must be one of {typing.get_args(hint)}, not {given!r}")
            ok = True
        else:
            (item, _) = typing.get_args(hint)
            allowed = typing.get_args(item) if typing.get_origin(item) is typing.Literal else None
            ok = isinstance(given, list) and all(isinstance(it, str) and (allowed is None or it in allowed) for it in given)
            if ok:
                value[name] = tuple(given)
            elif allowed:
                sys.exit(f"smoke.py: INPUT field {name} is a list of {', '.join(map(repr, allowed))}, not {given!r}")
        if not ok:
            sys.exit(f"smoke.py: INPUT field {name} is a {'string' if hint is str else 'list of strings'}, not {given!r}")
        if name in ("groups", "profiles") and not given:
            sys.exit(f"smoke.py: INPUT field {name} must not be empty")
    return Input(**value)


def archive_limit(maximum):
    """The child kernel refuses archive writes beyond the budget, before allocation.

    This is only the source-reading git child, on the Linux/macOS smoke hosts. Its
    inherited file-size bound may be stricter; never raise that bound for a read.
    The controller and compiler retain their own resource limits unchanged.
    """
    import resource
    soft, hard = resource.getrlimit(resource.RLIMIT_FSIZE)
    limit = min([maximum, *(value for value in (soft, hard) if value != resource.RLIM_INFINITY)])
    resource.setrlimit(resource.RLIMIT_FSIZE, (limit, hard))


def git(*words, output=None, max_bytes=None):
    """A bounded source read; a missing revision is never a newer-case skip."""
    if (output is None) != (max_bytes is None):
        raise ValueError("smoke corpus: a file output needs an explicit acquisition byte bound")
    if max_bytes is not None and (type(max_bytes) is not int or max_bytes <= 0):
        raise ValueError("smoke corpus: the archive acquisition limit must be a positive integer")
    if max_bytes is not None and os.name != "posix":
        raise ValueError("smoke corpus: bounded archive acquisition needs a Linux or macOS smoke host")
    done = subprocess.run(["git", "-C", ROOT, *words], stdout=output or subprocess.PIPE,
                          stderr=subprocess.PIPE, timeout=60,
                          preexec_fn=(lambda: archive_limit(max_bytes)) if max_bytes is not None else None)
    if max_bytes is not None and output.tell() > max_bytes:
        raise ValueError(f"smoke corpus archive acquisition exceeded {max_bytes} bytes")
    if done.returncode:
        limit = f" (archive acquisition limited to {max_bytes} bytes)" if max_bytes is not None else ""
        raise ValueError(f"smoke corpus: git {shlex.join(words)} exited {done.returncode}{limit}: "
                         f"{done.stderr.decode(errors='replace').strip()}")
    return done.stdout


def resolve_commit(revision):
    commit = git("rev-parse", "--verify", "--end-of-options", f"{revision}^{{commit}}").decode().strip()
    if len(commit) not in (40, 64) or any(c not in "0123456789abcdef" for c in commit):
        raise ValueError(f"smoke corpus: {revision!r} did not resolve to a full commit: {commit!r}")
    return commit


def corpus_files(root):
    """Only bounded regular files, with no link or special-file escape from the snapshot."""
    found, total, count = [], 0, 0
    for directory, directories, names in os.walk(root):
        directories[:] = sorted(name for name in directories if name != "target")
        names = sorted(name for name in names if name != "target")
        count += len(directories) + len(names)
        if count > CORPUS_FILES:
            raise ValueError(f"smoke corpus exceeds {CORPUS_FILES} entries")
        for name in directories:
            path = Path(directory, name)
            if path.is_symlink():
                raise ValueError(f"smoke corpus: directory link {path} is not a snapshot file")
        for name in names:
            path = Path(directory, name)
            info = path.lstat()
            if not stat.S_ISREG(info.st_mode):
                raise ValueError(f"smoke corpus: {path} is not a regular snapshot file")
            total += info.st_size
            found.append(path)
            if total > CORPUS_BYTES or len(found) > CORPUS_FILES:
                raise ValueError(f"smoke corpus exceeds {CORPUS_BYTES} bytes or {CORPUS_FILES} files")
    return sorted(found)


def file_digest(path):
    digest = hashlib.sha256()
    with open(path, "rb") as source:
        for block in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(block)
    return digest.hexdigest()


def manifest_at(root):
    path = Path(root, "smoke.json")
    with path.open(encoding="utf-8") as source:
        manifest = json.load(source)
    if not isinstance(manifest, dict) or not all(isinstance(manifest.get(k), dict) for k in ("cases", "projects")):
        raise ValueError(f"smoke corpus: {path} needs cases and projects objects")
    if set(manifest["cases"]) & set(manifest["projects"]):
        raise ValueError(f"smoke corpus: {path} names an item as both a case and a project")
    for kind in ("cases", "projects"):
        for name, spec in manifest[kind].items():
            if not name or name in (".", "..") or "/" in name or "\\" in name or not isinstance(spec, dict):
                raise ValueError(f"smoke corpus: invalid {kind} entry {name!r} in {path}")
            if spec.get("group", "projects" if kind == "projects" else None) not in typing.get_args(Group):
                raise ValueError(f"smoke corpus: {kind} entry {name!r} has no valid group in {path}")
            if kind == "cases" and (type(spec.get("tests")) is not int or spec["tests"] < 0):
                raise ValueError(f"smoke corpus: case {name!r} needs a nonnegative tests count in {path}")
            if kind == "projects":
                project_checks(name, spec, path)
                if "macos" in spec:
                    if not isinstance(spec["macos"], dict):
                        raise ValueError(f"smoke corpus: project {name!r} needs an object for macos in {path}")
                    project_checks(name, {**spec, **spec["macos"]}, path)
    return manifest


def project_checks(name, spec, path):
    """Fetching or preparing a source tree alone cannot establish a compiler check."""
    for field in ("builds", "runs"):
        if field in spec and (not isinstance(spec[field], list)
                              or not all(isinstance(step, dict) for step in spec[field])):
            raise ValueError(f"smoke corpus: project {name!r} needs a list of {field} steps in {path}")
    if "test" in spec and (not isinstance(spec["test"], list)
                           or not all(isinstance(word, str) for word in spec["test"])):
        raise ValueError(f"smoke corpus: project {name!r} needs a test argument list in {path}")
    if not spec.get("builds") and not spec.get("runs") and "test" not in spec:
        raise ValueError(f"smoke corpus: project {name!r} has no build, run or test check in {path}")


class Corpus:
    """One immutable input snapshot; selection and execution read the same manifest and files."""
    def __init__(self, revision):
        self.temporary = tempfile.TemporaryDirectory(prefix="blaze-smoke-corpus-")
        self.root = Path(self.temporary.name, "smoke")
        self.commit = self.tree = ""
        try:
            if revision:
                self.commit = resolve_commit(revision)
                self.tree = git("rev-parse", "--verify", f"{self.commit}:harness/smoke").decode().strip()
                self.root.mkdir()
                with tempfile.TemporaryFile() as archive:
                    git("archive", "--format=tar", f"{self.commit}:harness/smoke", output=archive,
                        max_bytes=CORPUS_BYTES)
                    if archive.tell() > CORPUS_BYTES:
                        raise ValueError(f"smoke corpus archive at {self.commit} exceeds {CORPUS_BYTES} bytes")
                    archive.seek(0)
                    self.extract(archive)
            else:
                corpus_files(CORPUS)
                shutil.copytree(CORPUS, self.root, symlinks=True, ignore=shutil.ignore_patterns("target"))
            files = corpus_files(self.root)
            self.manifest = manifest_at(self.root)
            for name in self.manifest["cases"]:
                for required in ("Cargo.toml", "expected.txt"):
                    if not Path(self.root, name, required).is_file():
                        raise ValueError(f"smoke corpus at {self.commit or CORPUS}: case {name!r} lacks {required}")
            rows = [(p.relative_to(self.root).as_posix(), p.stat().st_mode & 0o777, file_digest(p)) for p in files]
            self.identity = dict(commit=self.commit or None, tree=self.tree or None,
                                 sha256=hashlib.sha256(json.dumps(rows, separators=(",", ":")).encode()).hexdigest())
        except BaseException:
            self.temporary.cleanup()
            raise

    def extract(self, archive):
        total = count = 0
        with tarfile.open(fileobj=archive, mode="r:") as contents:
            for member in contents:
                relative = PurePosixPath(member.name)
                if (relative.is_absolute() or ".." in relative.parts or "\\" in member.name
                        or not relative.parts or ":" in relative.parts[0]):
                    raise ValueError(f"smoke corpus: unsafe archive path {member.name!r}")
                if not member.isdir() and not member.isfile():
                    raise ValueError(f"smoke corpus: archive member {member.name!r} is not a regular file or directory")
                if member.size < 0:
                    raise ValueError(f"smoke corpus: archive member {member.name!r} has a negative size")
                total += member.size
                count += 1
                if total > CORPUS_BYTES or count > CORPUS_FILES:
                    raise ValueError(f"smoke corpus exceeds {CORPUS_BYTES} bytes or {CORPUS_FILES} entries")
                path = self.root.joinpath(*relative.parts)
                if member.isdir():
                    path.mkdir(parents=True, exist_ok=True)
                else:
                    path.parent.mkdir(parents=True, exist_ok=True)
                    with contents.extractfile(member) as source, path.open("xb") as destination:
                        shutil.copyfileobj(source, destination)
                    path.chmod(0o755 if member.mode & 0o111 else 0o644)

    def __enter__(self):
        return self

    def __exit__(self, *_):
        self.temporary.cleanup()


def selection(args, corpus):
    manifest = corpus.manifest
    newer = {}
    if corpus.commit:
        newer = {name: spec for name, spec in manifest_at(CORPUS)["cases"].items()
                 if name not in manifest["cases"] and name not in manifest["projects"]}
    named = set(manifest["cases"]) | set(manifest["projects"]) | set(newer)
    if unknown := sorted(set(args.only) - named):
        raise ValueError(f"smoke.py: INPUT field only: no case or project {', '.join(unknown)}")

    def selected(items, default):
        return {n: s for n, s in items.items()
                if s.get("group", default) in args.groups and (not args.only or n in args.only)}

    cases = selected(manifest["cases"], None)
    projects = selected(manifest["projects"], "projects")
    skipped = selected(newer, None)
    if left := sorted(set(args.only) - set(cases) - set(projects) - set(skipped)):
        raise ValueError(f"smoke.py: INPUT field only names {', '.join(left)}, outside INPUT's groups {args.groups}")
    if not cases and not projects:
        raise ValueError(f"smoke corpus at {corpus.commit or CORPUS}: selection has no cases or projects to run")
    return cases, projects, skipped


def blaze_program(given):
    found = given or shutil.which("blaze")
    if not found or not os.path.isfile(found):
        sys.exit(f"smoke.py: no blaze {found or 'on PATH'}: INPUT's `blaze` names a build's, or an installed one is on PATH")
    return str(Path(found).resolve(strict=True))


def compiler_identity(blaze):
    """Bind the selected program and the optional companion shipped in releases.

    Both current V2 programs embed blaze_engine::main, so a standalone selected
    program is valid. A companion present in a release is still part of that
    executable set: its disappearance, appearance, replacement or drift is refused.
    """
    selected = Path(blaze)
    companion = selected.with_name("blaze-engine" + EXE)
    engine = None
    if companion != selected and (companion.exists() or companion.is_symlink()):
        resolved = companion.resolve(strict=True)
        engine = dict(path=str(resolved), sha256=file_digest(resolved))
    return dict(path=str(selected), sha256=file_digest(selected), engine=engine)


def base_env():
    # BLAZE_LOCK: the cases and projects are copies, throwaway workspaces with no committed lock: their builds may write lock files under CI (docs/cargo.md, "Lock files"); a typed --locked still wins.
    env = dict(os.environ, CARGO_TERM_COLOR="never", RUST_BACKTRACE="0", BLAZE_LOCK="write")
    for name in ("RUSTFLAGS", "CARGO_TARGET_DIR", "CARGO_BUILD_TARGET", "RUSTC_WRAPPER", "RUSTC_WORKSPACE_WRAPPER"):
        env.pop(name, None)
    return env


def shown(argv, env=None, cwd=None):
    """The command as a shell line a person can paste."""
    sets = [f"{k}={shlex.quote(v)}" for k, v in (env or {}).items()]
    line = " ".join(sets + [shlex.quote(a) for a in argv])
    return f"cd {shlex.quote(cwd)} && {line}" if cwd else line


def run(argv, cwd, env, timeout):
    began = time.monotonic()
    try:
        done = subprocess.run(argv, cwd=cwd, env=env, capture_output=True, timeout=timeout)
        code, out, err = done.returncode, done.stdout.decode(errors="replace"), done.stderr.decode(errors="replace")
    except subprocess.TimeoutExpired as e:
        code, out = None, (e.stdout or b"").decode(errors="replace")
        err = (e.stderr or b"").decode(errors="replace") + f"\n(no answer after {timeout} s)"
    return code, out, err, time.monotonic() - began


def test_counts(out):
    """(passed, failed) summed over libtest's `test result:` lines."""
    passed = failed = 0
    for line in out.splitlines():
        words = line.split()
        if line.startswith("test result:") and "passed;" in words and "failed;" in words:
            passed += int(words[words.index("passed;") - 1])
            failed += int(words[words.index("failed;") - 1])
    return passed, failed


def tail(text, lines=25):
    return "\n".join(text.rstrip().splitlines()[-lines:])


def fetch(name, spec, rev, dest):
    """`dest` checked out at `rev` of `spec["repo"]`, fetched alone (no history)."""
    if os.path.isdir(os.path.join(dest, ".git")):
        head = subprocess.run(["git", "-C", dest, "rev-parse", "HEAD"], capture_output=True, text=True).stdout.strip()
        if head == rev:
            return
        shutil.rmtree(dest)
    os.makedirs(dest)
    auth = []
    if spec.get("private"):
        token = os.environ.get("GH_TOKEN") or os.environ.get("GITHUB_TOKEN")
        if not token:
            raise RuntimeError(f"{spec['repo']} is private: set GH_TOKEN to a token that reads it")
        basic = base64.b64encode(f"x-access-token:{token}".encode()).decode()
        auth = ["-c", f"http.https://github.com/.extraheader=AUTHORIZATION: basic {basic}"]
    for argv in (["git", "init", "-q", dest],
                 ["git", *auth, "-C", dest, "fetch", "-q", "--depth", "1", f"https://github.com/{spec['repo']}", rev],
                 ["git", "-C", dest, "checkout", "-q", "FETCH_HEAD"]):
        done = subprocess.run(argv, capture_output=True, text=True, timeout=1800)
        if done.returncode:
            raise RuntimeError(f"{name}: `{' '.join(argv[-3:])}` failed: {done.stderr.strip()}")


class Smoke:
    def __init__(self, args, corpus):
        self.blaze = blaze_program(args.blaze)
        self.compiler = compiler_identity(self.blaze)
        self.work = os.path.abspath(args.work or tempfile.mkdtemp(prefix="blaze-smoke-"))
        os.makedirs(self.work, exist_ok=True)
        self.profiles = args.profiles
        self.blaze_rev = args.blaze_rev
        self.corpus = corpus
        self.results = []

    def argv(self, *words):
        return [self.blaze, "--ignore-pin", *words]

    def stalled(self, cwd, env):
        """What blaze says after a command gave no answer in its time: `blaze slow` (the span
        that names the slow thing), then `blaze stop`. Each is bounded, and each one's exit and
        output are kept with the result."""
        said = []
        for words, secs in ((["slow"], 300), (["stop"], 120)):
            command = self.argv(*words)
            try:
                code, out, err, _ = run(command, cwd, env, secs)
                said.append(f"--- {shown(command)}: exit {code}\n{out}{err}")
            except (OSError, subprocess.SubprocessError) as error:
                said.append(f"--- {shown(command)} could not finish: {error}")
        return "\n".join(said)

    def stop(self):
        command = self.argv("stop")
        self.check("daemon stop", command, self.work, base_env(), 120,
                   lambda code, _: None if code == 0 else f"stop exited {code}",
                   shown(command), None, diagnose=False)

    def error(self, what, error):
        self.results.append(dict(what=what, ok=False, open=None, seconds=0, why=str(error),
                                 repro="", stdout="", stderr=str(error)))
        print(f"FAILED   {what}: {error}", flush=True)

    def check(self, what, argv, cwd, env, timeout, judge, repro, open_issue, diagnose=True):
        code, out, err, secs = run(argv, cwd, env, timeout)
        if code is None and diagnose:
            err += "\n" + self.stalled(cwd, env)
        lines = 25 if code is not None else 80
        not_ours = [line for line in err.splitlines() if any(it in line for it in NOT_BLAZES)]
        why = f"the build was not blaze's: {not_ours[0].strip()}" if code == 0 and not_ours else judge(code, out)
        result = dict(what=what, ok=why is None, open=open_issue, seconds=round(secs, 1), why=why,
                      repro=repro, stdout=tail(out, lines), stderr=tail(err, lines))
        self.results.append(result)
        if why is None:
            mark = "ok, OPEN passes now (remove its mark)" if open_issue else "ok"
        else:
            mark = f"OPEN ({open_issue})" if open_issue else "FAILED"
        print(f"{mark:<8} {what} ({secs:.1f} s)", flush=True)
        if why is not None:
            print(f"    {why}\n    repeat: {repro}", flush=True)
            for label, text in (("stdout", out), ("stderr", err)):
                if text.strip():
                    print(f"    --- {label}\n" + "\n".join("    " + line for line in tail(text, lines).splitlines()), flush=True)

    def profile_args(self, profile):
        return ["--release"] if profile == "release" else []

    def case(self, name, spec):
        source = os.path.join(self.corpus.root, name)
        cwd = os.path.join(self.work, "cases", name)
        shutil.rmtree(cwd, ignore_errors=True)
        shutil.copytree(source, cwd, ignore=shutil.ignore_patterns("target"))
        with open(os.path.join(source, "expected.txt"), encoding="utf-8") as answer:
            expected = answer.read()
        extra = spec.get("env", {})
        env = dict(base_env(), **extra)
        tests = spec["tests"]

        def ran(code, out):
            if code != 0:
                return f"exit {code}"
            if out != expected:
                return "stdout is not expected.txt:\n" + "".join(
                    f"      want {w!r}\n      got  {g!r}\n" for w, g in zip_longest(expected, out) if w != g)
            return None

        def tested(code, out):
            passed, failed = test_counts(out)
            if code != 0 or failed:
                return f"exit {code}, {passed} passed, {failed} failed"
            if passed != tests:
                return f"{passed} tests passed, {tests} expected"
            return None

        for profile in self.profiles:
            p = self.profile_args(profile)
            argv = self.argv("run", *p, "--", *spec.get("args", []))
            repro = shown(argv, extra, cwd)
            self.check(f"{name} {profile} run", argv, cwd, env, CASE_SECS, ran, repro, spec.get("open"))
            argv = self.argv("test", *p)
            self.check(f"{name} {profile} test", argv, cwd, env, CASE_SECS, tested,
                       shown(argv, extra, cwd), spec.get("open"))

    def project(self, name, spec):
        if sys.platform == "darwin":
            spec = {**spec, **spec.get("macos", {})}
        for left_out, why in spec.get("excluded", {}).items():
            print(f"excluded {name}: {left_out}: {why}", flush=True)
        rev = spec["rev"].replace("{blaze}", self.blaze_rev)
        cwd = os.path.join(self.work, "projects", name)
        out_dir = os.path.join(self.work, "out", name)
        os.makedirs(out_dir, exist_ok=True)
        clone = f"git clone {shlex.quote('https://github.com/' + spec['repo'])} {shlex.quote(name)} && cd {shlex.quote(name)} && git checkout {shlex.quote(rev)}"
        try:
            fetch(name, spec, rev, cwd)
        except (RuntimeError, subprocess.TimeoutExpired) as e:
            self.results.append(dict(what=f"{name} fetch", ok=False, open=spec.get("open"), seconds=0,
                                     why=str(e), repro=clone, stdout="", stderr=""))
            print(f"FAILED   {name} fetch\n    {e}", flush=True)
            return
        env = dict(base_env(), **spec.get("env", {}))
        if "prepare" in spec:
            began = time.monotonic()
            try:
                cwd, prepared = importlib.import_module(spec["prepare"]).prepare(cwd)
            except (Exception, SystemExit) as e:
                prepare_input = shlex.quote(json.dumps({"checkout": "."})) if spec["prepare"] == "bunprep" else "."
                prepare_script = shlex.quote(os.path.join(HERE, f"{spec['prepare']}.py"))
                self.results.append(dict(what=f"{name} prepare", ok=False, open=spec.get("open"),
                                         seconds=round(time.monotonic() - began, 1), why=str(e)[-3000:],
                                         repro=f"{clone} && python3 {prepare_script} {prepare_input}", stdout="", stderr=""))
                print(f"FAILED   {name} prepare\n    {str(e)[-3000:]}", flush=True)
                return
            env.update(prepared)
            print(f"ok       {name} prepare ({time.monotonic() - began:.1f} s)", flush=True)
        if "dir" in spec:
            cwd = os.path.join(cwd, spec["dir"])
            clone = f"{clone} && cd {spec['dir']}"
        seconds = spec.get("seconds", PROJECT_SECS)
        places = {"{work}": out_dir, "{input}": os.path.join(out_dir, "input.txt")}
        if "input" in spec:
            with open(places["{input}"], "w") as f:
                f.write(spec["input"])

        def fill(args):
            for key, value in places.items():
                args = [a.replace(key, value) for a in args]
            return args

        def judged(step):
            def judge(code, out):
                if code != 0:
                    return f"exit {code}"
                if "stdout" in step and out != step["stdout"]:
                    return f"stdout {out!r}, want {step['stdout']!r}"
                if "contains" in step and step["contains"] not in out:
                    return f"stdout has no {step['contains']!r}"
                return None
            return judge

        for i, selection in enumerate(spec.get("fetches", [])):
            argv = ["lock", "--fetches", *selection]
            self.check(f"{name} lock {i}", self.argv(*argv), cwd, env, seconds, judged({}),
                       f"{clone} && {shown(self.argv(*argv), spec.get('env'))}", spec.get("open"))
        for profile in self.profiles:
            p = self.profile_args(profile)
            for i, step in enumerate(spec.get("builds", [])):
                at = os.path.join(cwd, step.get("dir", "."))
                built = shown(self.argv("build", *p, *step.get("args", [])), spec.get("env"), step.get("dir"))
                self.check(f"{name} {profile} build {i}", self.argv("build", *p, *step.get("args", [])), at, env,
                           seconds, judged({}), f"{clone} && {built}", spec.get("open"))
                if "then" in step:
                    out_of = os.path.join("target", "release" if profile == "release" else "debug")
                    then = [a.replace("{target}", os.path.join(at, out_of)) for a in fill(step["then"])]
                    self.check(f"{name} {profile} build {i} then", then, at, env, seconds, judged(step),
                               f"{clone} && {built} && {shown([a.replace('{target}', out_of) for a in step['then']])}",
                               spec.get("open"))
            for i, step in enumerate(spec.get("runs", [])):
                args = [*p, *step.get("select", []), "--", *fill(step["args"])]
                file = fill([step["file"]])[0] if "file" in step else None
                if file and os.path.exists(file):
                    os.remove(file)

                def ran(code, out, step=step, file=file):
                    if why := judged(step)(code, out):
                        return why
                    if file:
                        if not os.path.isfile(file):
                            return f"it wrote no {file}"
                        with open(file, "rb") as f:
                            if not f.read(len(step["starts"])) == step["starts"].encode("latin-1"):
                                return f"{file} does not start with {step['starts']!r}"
                    return None

                self.check(f"{name} {profile} run {i}", self.argv("run", *args), cwd, env, seconds, ran,
                           f"{clone} && {shown(self.argv('run', *args), spec.get('env'))}", spec.get("open"))
            if "test" in spec:
                args = [*p, *spec["test"]]

                def tested(code, out):
                    passed, failed = test_counts(out)
                    if code != 0 or failed:
                        return f"exit {code}, {passed} passed, {failed} failed"
                    return None if passed else "no test ran"

                self.check(f"{name} {profile} test", self.argv("test", *args), cwd, env, seconds, tested,
                           f"{clone} && {shown(self.argv('test', *args), spec.get('env'))}", spec.get("open"))


def zip_longest(a, b):
    """The lines of `a` and `b` side by side, '' past the end of the shorter."""
    a, b = a.splitlines(keepends=True), b.splitlines(keepends=True)
    return [(a[i] if i < len(a) else "", b[i] if i < len(b) else "") for i in range(max(len(a), len(b)))]


def summary(results, out):
    failed = [r for r in results if not r["ok"] and not r["open"]]
    lines = ["| | what | s |", "|---|---|---|"]
    for r in results:
        mark = ("skipped" if r.get("skipped") else "ok") if r["ok"] else (f"open {r['open']}" if r["open"] else "**FAILED**")
        lines.append(f"| {mark} | {r['what']} | {r['seconds']} |")
    for r in failed:
        lines += ["", f"**{r['what']}**: {r['why']}", "", "```sh", r["repro"], "```"]
    out.write("\n".join(lines) + "\n")
    return failed


def execute(args, corpus):
    cases, projects, skipped = selection(args, corpus)
    if args.command in ("apt", "brew"):
        print(" ".join(sorted({p for s in projects.values() for p in s.get(args.command, [])})))
        return 0
    if not args.blaze_rev:
        args = replace(args, blaze_rev=resolve_commit("HEAD"))
    smoke = Smoke(args, corpus)
    version = ""
    began = time.monotonic()
    try:
        command = smoke.argv("--version")
        code, out, err, _ = run(command, smoke.work, base_env(), 60)
        if code != 0 or not out.strip():
            raise ValueError(f"{shown(command)} exited {code} without a valid version: {out}{err}")
        version = out.strip().splitlines()[0]
        print(f"{version}: {len(cases)} cases, {len(projects)} projects, {', '.join(args.profiles)}; work {smoke.work}", flush=True)
        print(f"compiler {json.dumps(smoke.compiler, sort_keys=True)}; corpus {json.dumps(corpus.identity, sort_keys=True)}", flush=True)
        for name, spec in skipped.items():
            why = f"absent from the manifest at {corpus.commit}: newer than the blaze under test"
            smoke.results.append(dict(what=name, ok=True, skipped=why, open=spec.get("open"), seconds=0,
                                      why=None, repro="", stdout="", stderr=""))
            print(f"skipped  {name}: {why}", flush=True)
        for name, spec in cases.items():
            smoke.case(name, spec)
        for name, spec in projects.items():
            smoke.project(name, spec)
    except (OSError, ValueError, TypeError, KeyError, subprocess.SubprocessError) as error:
        smoke.error("smoke harness", error)
    finally:
        try:
            smoke.stop()
        except (OSError, subprocess.SubprocessError) as error:
            smoke.error("daemon stop", error)
    try:
        if compiler_identity(smoke.blaze) != smoke.compiler:
            smoke.error("compiler identity", f"selected compiler or companion of {smoke.blaze} changed during the smoke run")
    except OSError as error:
        smoke.error("compiler identity", error)
    print(f"{time.monotonic() - began:.0f} s", flush=True)
    if args.results:
        with open(args.results, "w") as f:
            json.dump(dict(blaze=version, compiler=smoke.compiler, corpus=corpus.identity,
                           results=smoke.results), f, indent=1)
    failed = summary(smoke.results, sys.stdout)
    if step_summary := os.environ.get("GITHUB_STEP_SUMMARY"):
        with open(step_summary, "a") as f:
            f.write(f"### smoke corpus: {version}\n\n")
            summary(smoke.results, f)
    return 1 if failed else 0


def main():
    words = sys.argv[1:]
    if len(words) > 1 or words[:1] in (["-h"], ["--help"]):
        sys.exit(__doc__)
    try:
        args = typed(words[0] if words else "")
        with Corpus(args.corpus_rev) as corpus:
            code = execute(args, corpus)
    except (OSError, ValueError, TypeError, KeyError, tarfile.TarError, subprocess.SubprocessError) as error:
        sys.exit(f"smoke.py: {error}")
    sys.exit(code)


if __name__ == "__main__":
    main()
