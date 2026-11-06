#!/usr/bin/env python3
"""Audit owned builds, edits, tests, doctests and checks without external Rust tools.

    norustc.py --owned-server PATH [--bin PATH] [--work DIR] [--json FILE]

Every scenario checks its expected exit status. Blocking stand-ins must log no rustc,
rustdoc, Cargo, rustup or official macro-server attempts. Linux also audits exec attempts;
on macOS the audit covers PATH and the configured tool paths. Completed scratch is removed;
failures retain their diagnostics. The historical count baseline is never read or changed.

The public stand_ins/runs/exec helpers remain available to other harnesses. Their default
counting mode is unchanged; this CLI uses their copy-free blocking mode exclusively.
"""
import argparse, json, os, re, shlex, shutil, subprocess, sys, tempfile, time
from pathlib import Path

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(HERE)
RA_JIT = os.environ.get("RA_JIT_BIN") or os.path.join(ROOT, "target/release/ra-jit")
TOOLCHAIN = open(os.path.join(HERE, "toolchain")).read().strip()
FIXTURE = os.path.join(HERE, "norustc", "fixture")
COUNTED = ("rustc", "rustdoc", "cargo", "rustup")
# How long the log has to be still, with no counted tool running, for a step to be over.
STILL = 4
# The variable a harness puts in the environment of every process of a run, so that a process
# the run started can be found after it was reparented (docs/distributed.md, P0.1's watchdog).
RUN_ID_ENV = "RA_JIT_RUN_ID"

# The stand-in for a tool: one line per run (the tool, its pid, its parent's command line, its
# arguments, the run it belongs to), then the real tool, which keeps the pid.
STAND_IN = """#!{shell}
parent=$(ps -o args= -p $PPID 2>/dev/null | cut -c1-300 | tr '\\t\\n' '  ')
printf '%s\\t%s\\t%s\\t%s\\t%s\\n' {tool} $$ "$parent" "$(echo "$*" | cut -c1-1500 | tr '\\t\\n' '  ')" "${{{run_id_env}-}}" >> {log}
exec {real} "$@"
"""

MAIN, LIB = "src/main.rs", "word/src/lib.rs"
# The scenarios, each a list of (step, cargo ra-jit arguments, edits, whether to wait for the
# daemon to be ready). An edit is (file, old, new), or (file, None, text to add).
SCENARIOS = [
    ("cold build", [("cold", ["build"], [], True)]),
    ("edits", [
        ("nothing edited", ["build"], [], False),
        ("a body", ["build"], [(MAIN, '"{} words', '"{} words!')], False),
        ("a library's body", ["build"], [(LIB, ".count()", ".filter(|it| !it.is_empty()).count()")], False),
        ("a new function", ["build"], [(MAIN, "fn main() {", "fn more() -> u32 {\n    3\n}\n\nfn main() {\n    more();")], False),
        ("an error", ["build"], [(MAIN, "    more();", '    let _wrong: u32 = "no";\n    more();')], False),
        ("the error mended", ["build"], [(MAIN, '    let _wrong: u32 = "no";\n', "")], False),
    ]),
    ("tests", [
        ("first test --no-run", ["test", "--no-run", "--workspace"], [], True),
        ("a body", ["test", "--no-run", "--workspace"], [(MAIN, '"{} words!', '"{} words!!')], False),
        ("a test added", ["test", "--no-run", "--workspace"], [(LIB, None, "\n#[test]\nfn added() {}\n")], False),
    ]),
    ("doctests", [
        ("first", ["test", "--doc", "-p", "word"], [], False),
        ("again", ["test", "--doc", "-p", "word"], [], False),
    ]),
    ("check", [("check", ["check"], [], False)]),
    # With no build of its flags loaded the daemon loads the workspace to check it.
    ("check unloaded", [
        ("the daemon stopped", ["stop"], [], False),
        ("check", ["check"], [], False),
        ("an error", ["check"], [(MAIN, "fn main() {", 'fn wrong() -> u32 {\n    "no"\n}\n\nfn main() {')], False),
        ("the error mended", ["check"], [(MAIN, 'fn wrong() -> u32 {\n    "no"\n}\n\n', "")], False),
        ("another package", ["check", "-p", "word"], [], False),
    ]),
]


def run(argv, **kwargs):
    return subprocess.run(argv, capture_output=True, text=True, stdin=subprocess.DEVNULL, **kwargs)


def external_execs(text):
    """Every external compiler/Cargo exec attempt, including split trace records."""
    started, paths = False, []
    calls = re.finditer(r'^\s*(?:(?:\d+|\[pid\s+\d+\])\s+)?(execve|seccomp|prctl)\((.*)$',
                        text, re.MULTILINE)
    for call in calls:
        name, args = call.groups()
        if name == "execve":
            started = True
            path = re.match(r'"([^"\n]+)"', args)
            if path and os.path.basename(path[1]).removesuffix(".real") in (
                    *COUNTED, "rust-analyzer-proc-macro-srv"):
                paths.append(path[1])
        elif started:
            option = args.partition(",")[0].strip()
            try:
                numeric = int(option, 0) & 0xffffffff
            except ValueError:
                numeric = None
            if ((name == "prctl" and (numeric == 22 or option == "PR_SET_SECCOMP"))
                    or (name == "seccomp" and (numeric in (0, 1)
                                              or option.startswith("SECCOMP_SET_MODE_")))):
                raise RuntimeError("exec audit cannot certify a seccomp filter change attempt")
    if not started:
        raise RuntimeError("exec audit contains no initial execve")
    return paths


def exec_trace(argv, trace, *, detached=False):
    """Keep exec attempts visible without ptrace stops on unrelated filesystem reads.

    Filter changes are traced too: a later filter can hide exec attempts from seccomp-bpf,
    so external_execs rejects that evidence. The tracer's own setup precedes the first exec.
    Raw arguments keep private thread names and filter data out of the trace.
    """
    return ["strace", "--seccomp-bpf", "-f", *(["-D"] if detached else []),
            "-e", "trace=execve,seccomp,prctl", "-e", "raw=prctl,seccomp",
            "-o", os.fspath(trace), *argv]


def traced_run(argv, trace, *, cwd, env, timeout=7200):
    """Trace product children without waiting for a long-lived daemon to exit.

    Detached strace keeps inherited stdio open. Use files rather than pipes so
    request completion, not daemon exit, determines when the client returns.
    The caller stops its daemon before the final trace census and cleanup.
    """
    if sys.platform != "linux":
        return subprocess.run(argv, cwd=cwd, env=env, timeout=timeout, capture_output=True,
                              text=True, errors="replace", stdin=subprocess.DEVNULL)
    stdout, stderr = str(trace) + ".stdout", str(trace) + ".stderr"
    with open(stdout, "w") as out, open(stderr, "w") as err:
        ran = subprocess.run(exec_trace(argv, trace, detached=True),
                             cwd=cwd, env=env, timeout=timeout, stdout=out, stderr=err,
                             stdin=subprocess.DEVNULL)
    with open(stdout, errors="replace") as out, open(stderr, errors="replace") as err:
        return subprocess.CompletedProcess(argv, ran.returncode, out.read(), err.read())


def real_toolchain():
    """Reference-only sysroot query, with rustup or the flake's direct pinned rustc."""
    selected = run(["rustc", f"+{TOOLCHAIN}", "--print", "sysroot"])
    if selected.returncode == 0:
        return selected.stdout.strip()
    # A Nix compiler is not a rustup proxy and does not understand +toolchain.
    # Verify its release before using it for the independent oracle.
    version = run(["rustc", "--version"], check=True).stdout
    if not version.startswith(f"rustc {TOOLCHAIN} "):
        raise RuntimeError(f"the reference compiler must be Rust {TOOLCHAIN}")
    return run(["rustc", "--print", "sysroot"], check=True).stdout.strip()


def reference_host(toolchain):
    """The independent oracle's actual host, never a product compilation request."""
    version = run([os.path.join(toolchain, "bin", "rustc"), "-vV"], check=True).stdout
    facts = dict(line.split(": ", 1) for line in version.splitlines() if ": " in line)
    if facts.get("release") != TOOLCHAIN or not facts.get("host"):
        raise RuntimeError(f"reference host requires Rust {TOOLCHAIN}: {version}")
    return facts["host"]


def stand_ins(work, log, run_id_env=RUN_ID_ENV, block=False, block_macro_server=False):
    """A copy of the pinned toolchain at `work/toolchain` whose rustc, rustdoc and cargo are
    stand-ins that append a line to `log` for each run and then run the real tool, which keeps
    the pid. Every other program of the toolchain is linked, and its libraries are copied, so
    that rustc, finding its sysroot by where its library is, finds the copy and the copy is
    what a build's `--print=sysroot` names.

    The line is tab-separated: the tool, its pid, its parent's command line, its argv and the
    value of `run_id_env`, which a run sets so that what it started can be found after it was
    reparented. Returns the directory to put first on `PATH` and to name in `RUSTC`, `RUSTDOC`
    and `CARGO`."""
    sysroot = real_toolchain()
    toolchain = os.path.join(work, "toolchain")
    marker = os.path.join(toolchain, ".stand-in-log")
    marker_text = log + ("\nblocked-v2" if block else "") + ("\nblocked-server-v2" if block_macro_server else "")
    if os.path.exists(marker) and open(marker).read().strip() == marker_text:
        return toolchain  # made for this log already: every blaze run of the work directory shares it
    if os.path.exists(toolchain):
        shutil.rmtree(toolchain)
    os.makedirs(os.path.join(toolchain, "bin"))
    os.makedirs(os.path.join(toolchain, "lib"))
    for entry in os.listdir(sysroot):
        if entry not in ("bin", "lib") and not (block_macro_server and entry == "libexec"):
            os.symlink(os.path.join(sysroot, entry), os.path.join(toolchain, entry))
    for entry in os.listdir(os.path.join(sysroot, "lib")):
        source, copy = os.path.join(sysroot, "lib", entry), os.path.join(toolchain, "lib", entry)
        if os.path.isdir(source) or block:
            os.symlink(source, copy)
        else:
            shutil.copy2(source, copy)
    for entry in os.listdir(os.path.join(sysroot, "bin")):
        source, copy = os.path.join(sysroot, "bin", entry), os.path.join(toolchain, "bin", entry)
        if entry not in COUNTED and not (block_macro_server and entry == "rust-analyzer-proc-macro-srv"):
            os.symlink(source, copy)
            continue
        if not block:
            shutil.copy2(source, copy + ".real")
        with open(copy, "w") as out:
            script = STAND_IN.format(shell=shutil.which("sh") or "/bin/sh", tool=entry, log=shlex.quote(log), real=shlex.quote(copy + ".real"),
                                    run_id_env=run_id_env)
            out.write(script.rsplit("exec ", 1)[0] + "exit 97\n" if block or entry == "rust-analyzer-proc-macro-srv" else script)
        os.chmod(copy, 0o755)
    if block_macro_server:
        source = os.path.join(sysroot, "libexec")
        destination = os.path.join(toolchain, "libexec")
        os.makedirs(destination)
        for name in sorted(set(os.listdir(source) if os.path.isdir(source) else []) | {"rust-analyzer-proc-macro-srv"}):
            path = os.path.join(destination, name)
            if name != "rust-analyzer-proc-macro-srv":
                os.symlink(os.path.join(source, name), path)
            else:
                script = STAND_IN.format(shell=shutil.which("sh") or "/bin/sh", tool=name, log=shlex.quote(log), real="unused",
                                        run_id_env=run_id_env)
                with open(path, "w") as output:
                    output.write(script.rsplit("exec ", 1)[0] + "exit 97\n")
                os.chmod(path, 0o755)
    # rustup is outside the toolchain's bin directory. Audit its query/discovery
    # processes too; no row is allowed to hide them by calling them noncompiling.
    rustup = None if block else shutil.which("rustup")
    if (block or rustup) and not os.path.exists(os.path.join(toolchain, "bin", "rustup")):
        copy = os.path.join(toolchain, "bin", "rustup")
        with open(copy, "w") as out:
            script = STAND_IN.format(shell=shutil.which("sh") or "/bin/sh", tool="rustup", log=shlex.quote(log),
                                    real=shlex.quote(os.path.realpath(rustup)) if rustup else "unused",
                                    run_id_env=run_id_env)
            out.write(script.rsplit("exec ", 1)[0] + "exit 97\n" if block else script)
        os.chmod(copy, 0o755)
    if block:
        for name in COUNTED:
            copy = os.path.join(toolchain, "bin", name)
            if os.path.exists(copy):
                continue
            script = STAND_IN.format(shell=shutil.which("sh") or "/bin/sh", tool=name,
                                    log=shlex.quote(log), real="unused", run_id_env=run_id_env)
            with open(copy, "w") as out:
                out.write(script.rsplit("exec ", 1)[0] + "exit 97\n")
            os.chmod(copy, 0o755)
    with open(marker, "w") as f:
        f.write(marker_text + "\n")
    return toolchain


def counted_toolchain(work, log):
    """The stand-ins of [`stand_ins`], for the scenarios below."""
    return stand_ins(work, log)


def runs(log):
    """The runs in `log`, in the order they happened: dicts of `tool`, `pid`, `parent`, `argv`
    and `run_id`. A line a stand-in could not write whole is skipped, not fatal."""
    found = []
    if not os.path.exists(log):
        return found
    for line in open(log, errors="replace"):
        parts = line.rstrip("\n").split("\t")
        if len(parts) < 4:
            continue
        tool, pid, parent, argv = parts[:4]
        found.append({"tool": tool, "pid": int(pid) if pid.isdigit() else None, "parent": parent,
                      "argv": argv, "run_id": parts[4] if len(parts) > 4 else ""})
    return found


def argv_list(argv):
    """`argv` as a list of words. The stand-in logs the shell's `$*`, which joins the words with
    spaces and cannot show one that had a space in it; that is enough to name what ran."""
    return argv.split()



def counts(lines):
    """The runs of each tool in lines of the log, and how many of rustc's only asked."""
    runs = {tool: 0 for tool in COUNTED}
    questions = 0
    for line in lines:
        tool, _, _, args = (line.split("\t") + ["", "", ""])[:4]
        runs[tool] = runs.get(tool, 0) + 1
        words = args.split()
        kinds = [w.split("=", 1)[1] for w in words if w.startswith("--print=")]
        kinds += [b for a, b in zip(words, words[1:]) if a == "--print"]
        printing = kinds and not any(k.startswith(("link-args", "native-static-libs")) for k in kinds)
        if tool == "rustc" and (printing or {"-vV", "-V", "--version"} & set(words)):
            questions += 1
    return runs, questions


def alive(pid):
    try:
        os.kill(pid, 0)
    except (ProcessLookupError, PermissionError):
        return False
    state = run(["ps", "-o", "stat=", "-p", str(pid)]).stdout.strip()
    return bool(state) and "Z" not in state


def settle(log, limit=600):
    """Waits until no counted tool runs and the log has been still for STILL seconds."""
    began, size, still = time.time(), -1, 0.0
    while time.time() - began < limit:
        lines = open(log).read().splitlines()
        pids = [int(line.split("\t")[1]) for line in lines[-200:] if line.count("\t") >= 3]
        busy = any(alive(pid) for pid in pids)
        now = os.path.getsize(log)
        still = still + 0.5 if (now == size and not busy) else 0.0
        size = now
        if still >= STILL:
            return
        time.sleep(0.5)


def apply(ws, edits):
    for path, old, new in edits:
        path = os.path.join(ws, path)
        text = open(path).read()
        assert old is None or old in text, (path, old)
        open(path, "w").write(text + new if old is None else text.replace(old, new, 1))


def blocking_toolchain(work, log):
    toolchain = stand_ins(str(work), str(log), block=True, block_macro_server=True)
    controls = []
    for tool in (*COUNTED, "rust-analyzer-proc-macro-srv"):
        folder = "libexec" if tool == "rust-analyzer-proc-macro-srv" else "bin"
        path = os.path.join(toolchain, folder, tool)
        control = run([path, "--version"])
        controls.append({"tool": tool, "path": path, "code": control.returncode})
        if control.returncode != 97:
            raise RuntimeError(f"blocking control failed: {controls}")
    if [row["tool"] for row in runs(str(log))] != [row["tool"] for row in controls]:
        raise RuntimeError("blocking controls were not all logged")
    (work / "tool-controls.json").write_text(json.dumps(controls, indent=2) + "\n")
    log.write_text("")
    return toolchain


def owned_environment(work, compiler, server, toolchain):
    env = {k: v for k, v in os.environ.items()
           if not k.startswith(("RA_JIT", "BLAZE", "RUSTC", "RUSTDOC"))
           and not k.endswith("RUSTFLAGS")}
    for key in ("CARGO_TARGET_DIR", "CARGO_BUILD_TARGET", "CARGO_ENCODED_RUSTFLAGS", "CARGO"):
        env.pop(key, None)
    env.update(RUSTUP_TOOLCHAIN=toolchain, RA_JIT_HOME=str(work / "home"),
               RA_JIT_CACHE=str(work / "store"), CARGO_TARGET_DIR=str(work / "target"),
               RA_JIT_PROC_MACRO_SERVER=str(server), RA_JIT_ANSWERS="own", BLAZE_ANSWERS="own",
               RA_JIT_OWN_RESOLVE="1", BLAZE_OWN_RESOLVE="1", RA_JIT_STD_FROM="off",
               PATH=os.path.join(toolchain, "bin") + os.pathsep + str(compiler.parent)
                    + os.pathsep + env.get("PATH", ""),
               CARGO_TERM_COLOR="never", RUST_BACKTRACE="0", CARGO_NET_OFFLINE="true",
               RA_JIT_MEMORY_MAX="16384M", RA_JIT_OWN_BASE_MEMORY="12288M", RA_JIT_SPARE="0")
    env[RUN_ID_ENV] = os.environ.get(RUN_ID_ENV) or f"norustc-{os.getpid()}-{time.time_ns()}"
    for name, tool in (("RUSTC", "rustc"), ("RUSTDOC", "rustdoc"), ("CARGO", "cargo")):
        env[name] = os.path.join(toolchain, "bin", tool)
    if os.environ.get("RA_JIT_BACKEND"):
        env["RA_JIT_BACKEND"] = os.environ["RA_JIT_BACKEND"]
    return env


def owned_audit(args):
    # Imported only by the CLI: stand-in users keep their import-time behavior.
    if HERE not in sys.path:
        sys.path.insert(0, HERE)
    from dist.owned_macro_server import runtime_record
    compiler = args.bin.resolve(strict=True)
    server = args.owned_server.resolve(strict=True)
    pins = runtime_record(server, compiler)
    if not pins["bootstrap_compiler_matches"]:
        raise RuntimeError("owned macro runtime was bootstrapped by a different compiler")
    if not os.access(compiler, os.X_OK) or not os.access(server, os.X_OK):
        raise RuntimeError("compiler and owned macro runtime must be executable")
    if args.work:
        work = args.work.resolve()
        work.mkdir(parents=True, exist_ok=False)
    else:
        work = Path(tempfile.mkdtemp(prefix="ra-jit-norustc-",
                                    dir="/tmp" if sys.platform == "darwin" else None)).resolve()
    result = {"runtime": pins, "work": str(work), "steps": [], "failures": [],
              "audit_scope": "all exec attempts" if sys.platform == "linux"
                             else "PATH and configured Rust tool paths"}
    log, traces = work / "runs.log", []
    env = None
    ws = work / "ws"

    def invoke(scenario, step, words, expected=0):
        number = len(result["steps"])
        trace = work / f"{number:02d}.exec"
        if sys.platform == "linux":
            traces.append(trace)
        row = {"scenario": scenario, "step": step, "args": words, "expected": expected,
               "stdout": str(trace) + ".stdout", "stderr": str(trace) + ".stderr"}
        result["steps"].append(row)
        began = time.monotonic()
        try:
            ran = traced_run([str(compiler), "cargo", *words], trace, cwd=str(ws),
                             env=env, timeout=args.timeout)
        except subprocess.TimeoutExpired as error:
            row["timed_out"] = True
            for field in ("stdout", "stderr"):
                output = getattr(error, field) or ""
                if not Path(row[field]).exists():
                    Path(row[field]).write_text(output.decode(errors="replace")
                                               if isinstance(output, bytes) else output)
            raise RuntimeError(f"{scenario}: {step} exceeded {args.timeout}s") from error
        finally:
            row["seconds"] = round(time.monotonic() - began, 3)
        row["code"] = ran.returncode
        Path(row["stdout"]).write_text(ran.stdout)
        Path(row["stderr"]).write_text(ran.stderr)
        print(f"  {scenario}: {step}: exit {ran.returncode} (expected {expected})", flush=True)
        if ran.returncode != expected:
            result["failures"].append(f"{scenario}: {step}: exit {ran.returncode}, expected {expected}")
        return ran

    try:
        log.write_text("")
        toolchain = blocking_toolchain(work, log)
        shutil.copytree(FIXTURE, ws)
        (work / "home").mkdir(mode=0o700)
        env = owned_environment(work, compiler, server, toolchain)
        for scenario, steps in SCENARIOS:
            for step, words, edits, wait in steps:
                apply(str(ws), edits)
                expected = 101 if (scenario, step) in (("edits", "an error"),
                                                      ("check unloaded", "an error")) else 0
                ran = invoke(scenario, step, words, expected)
                if expected == 101 and not re.search(r"(?:^|: )error\[E0308\]:", ran.stderr, re.M):
                    result["failures"].append(f"{scenario}: {step}: missing intentional type-error diagnostic")
                if wait and ran.returncode == 0:
                    status = invoke(scenario, step + " status", ["status"])
                    if ": ready" not in status.stdout:
                        result["failures"].append(f"{scenario}: {step}: daemon is not ready")
    except Exception as error:
        result["failures"].append(f"{type(error).__name__}: {error}")
    finally:
        if env is not None:
            try:
                invoke("cleanup", "stop", ["stop"])
            except Exception as error:
                result["failures"].append(f"daemon stop failed: {error}")
        result["tool_calls"] = runs(str(log))
        if log.exists() and log.read_text().strip():
            result["failures"].append("prohibited tool attempts logged in runs.log")
        for trace in traces:
            try:
                calls = external_execs(trace.read_text(errors="replace"))
                if calls:
                    result["failures"].append(f"prohibited exec attempts in {trace.name}: {calls}")
            except Exception as error:
                result["failures"].append(f"exec audit failed for {trace.name}: {error}")
        try:
            if runtime_record(server, compiler) != pins:
                result["failures"].append("compiler or owned macro runtime changed during audit")
        except Exception as error:
            result["failures"].append(f"runtime pin recheck failed: {error}")
        summary = json.dumps(result, indent=2) + "\n"
        (work / "summary.json").write_text(summary)
        if args.json:
            args.json.resolve().write_text(summary)
    if result["failures"]:
        for failure in result["failures"]:
            print(f"FAILED: {failure}", file=sys.stderr)
        print(f"diagnostics retained at {work}", file=sys.stderr)
        return 1
    shutil.rmtree(work)
    print(f"passed {len(result['steps'])} steps; zero prohibited tool calls; scratch removed")
    return 0


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--bin", type=Path, default=Path(RA_JIT))
    parser.add_argument("--owned-server", type=Path, required=True)
    parser.add_argument("--work", type=Path)
    parser.add_argument("--json", type=Path)
    parser.add_argument("--timeout", type=int, default=1200, help="seconds per command")
    args = parser.parse_args()
    if args.timeout <= 0:
        parser.error("--timeout must be positive")
    try:
        return owned_audit(args)
    except Exception as error:
        print(f"FAILED: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main())
