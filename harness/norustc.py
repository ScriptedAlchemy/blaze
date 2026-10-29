#!/usr/bin/env python3
"""norustc.py: how many times rustc and rustdoc run in each scenario of a small workspace built
with `cargo ra-jit`, against the counts recorded in norustc/baseline.json. A count above its
baseline fails: ra-jit is to run no rustc, and a change may only lower the counts
(docs/no-rustc.md).

    norustc.py [--record] [--keep]

It is also the module of the stand-ins: [`stand_ins`] writes a `rustc`, `rustdoc` and `cargo`
that log each run (the tool, its pid, its parent's command line, its argv) and then run the real
tool, and [`runs`] reads that log back. `harness/tracedecay.py` puts them on `PATH` and in
`RUSTC`, `RUSTDOC` and `CARGO` for a blaze run, so that a run of rustc or rustdoc by blaze, by
cargo or by anything they start is counted and can be named (P0-A, docs/distributed.md).

The workspace, norustc/fixture, has a program with an `asm!` block, a library with unit tests,
doctests and features, which the root package uses through another library, a proc macro, and a path dependency outside the workspace with a build script that
`include!`s a file, `include_str!`s another, reads a variable with `option_env!` and expands the proc macro. It
is built with a copy of the pinned toolchain whose rustc, rustdoc and cargo write a line for
each run and then become the real tool, linked with rustup under a name of its own for the run.
A scenario ends once no counted tool runs and the log has been still for a while, so what the
daemon does after a command is counted with it.

--record writes this platform's and backend's counts into the baseline, after a change that
lowered them. Where a count differs from run to run (cargo's own cache of what rustc answered
is raced by the cargos ra-jit runs beside it), the baseline is the highest seen. --keep leaves the work directory. Linux and macOS: the stand-ins are shell
scripts.
"""
import json, os, platform, re, shlex, shutil, subprocess, sys, tempfile, time

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(HERE)
RA_JIT = os.environ.get("RA_JIT_BIN") or os.path.join(ROOT, "target/release/ra-jit")
TOOLCHAIN = open(os.path.join(HERE, "toolchain")).read().strip()
FIXTURE = os.path.join(HERE, "norustc", "fixture")
BASELINE = os.path.join(HERE, "norustc", "baseline.json")
SYSTEM = {"linux": "linux", "darwin": "macos"}[sys.platform]
ARCH = "aarch64" if platform.machine().lower() in ("arm64", "aarch64") else "x86_64"
PLACE = f"{SYSTEM}-{ARCH} {os.environ.get('RA_JIT_BACKEND') or 'cranelift'}"
COUNTED = ("rustc", "rustdoc", "cargo", "rustup")
# How long the log has to be still, with no counted tool running, for a step to be over.
STILL = 4
# The variable a harness puts in the environment of every process of a run, so that a process
# the run started can be found after it was reparented (docs/distributed.md, P0.1's watchdog).
RUN_ID_ENV = "RA_JIT_RUN_ID"

# The stand-in for a tool: one line per run (the tool, its pid, its parent's command line, its
# arguments, the run it belongs to), then the real tool, which keeps the pid.
STAND_IN = """#!/bin/sh
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
    return [path for path in re.findall(r'execve\("([^"\n]+)"', text)
            if os.path.basename(path).removesuffix(".real") in (*COUNTED, "rust-analyzer-proc-macro-srv")]


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
        ran = subprocess.run(["strace", "-f", "-D", "-e", "trace=execve", "-o", str(trace), *argv],
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
    marker_text = log + ("\nblocked" if block else "") + ("\nblocked-server" if block_macro_server else "")
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
        if entry not in COUNTED:
            os.symlink(source, copy)
            continue
        if not block:
            shutil.copy2(source, copy + ".real")
        with open(copy, "w") as out:
            script = STAND_IN.format(tool=entry, log=shlex.quote(log), real=shlex.quote(copy + ".real"),
                                    run_id_env=run_id_env)
            out.write(script.rsplit("exec ", 1)[0] + "exit 86\n" if block else script)
        os.chmod(copy, 0o755)
    if block_macro_server:
        source = os.path.join(sysroot, "libexec")
        destination = os.path.join(toolchain, "libexec")
        os.makedirs(destination)
        for name in os.listdir(source) if os.path.isdir(source) else []:
            path = os.path.join(destination, name)
            if name != "rust-analyzer-proc-macro-srv":
                os.symlink(os.path.join(source, name), path)
            else:
                script = STAND_IN.format(tool=name, log=shlex.quote(log), real="unused",
                                        run_id_env=run_id_env)
                with open(path, "w") as output:
                    output.write(script.rsplit("exec ", 1)[0] + "exit 86\n")
                os.chmod(path, 0o755)
    # rustup is outside the toolchain's bin directory. Audit its query/discovery
    # processes too; no row is allowed to hide them by calling them noncompiling.
    rustup = shutil.which("rustup")
    if rustup and not os.path.exists(os.path.join(toolchain, "bin", "rustup")):
        copy = os.path.join(toolchain, "bin", "rustup")
        with open(copy, "w") as out:
            script = STAND_IN.format(tool="rustup", log=shlex.quote(log),
                                    real=shlex.quote(os.path.realpath(rustup)),
                                    run_id_env=run_id_env)
            out.write(script.rsplit("exec ", 1)[0] + "exit 86\n" if block else script)
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


def main():
    record, keep = "--record" in sys.argv, "--keep" in sys.argv
    # Under /tmp on macOS: the daemon's socket is in RA_JIT_HOME below, and under $TMPDIR
    # (/var/folders/...) its path passes the 104 bytes macOS has for one.
    work = tempfile.mkdtemp(prefix="ra-jit-norustc-", dir="/tmp" if sys.platform == "darwin" else None)
    log = os.path.join(work, "runs.log")
    open(log, "w").close()
    name = f"ra-jit-norustc-{os.getpid()}"
    toolchain = counted_toolchain(work, log)
    run(["rustup", "toolchain", "link", name, toolchain], check=True)
    ws = os.path.join(work, "ws")
    shutil.copytree(FIXTURE, ws)
    home = os.path.join(work, "home")
    os.makedirs(home, mode=0o700)
    env = {k: v for k, v in os.environ.items() if not k.startswith(("RA_JIT", "RUSTC", "RUSTDOC"))}
    for unset in ("RUSTFLAGS", "CARGO_TARGET_DIR", "CARGO_BUILD_TARGET", "CARGO"):
        env.pop(unset, None)
    # Flags the shell gives a target (nix's x86-64 Linux shell sets
    # CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUSTFLAGS) have cargo ask rustc about the target again
    # under them: six more runs that only ask in the cold build.
    for key in [k for k in env if k.endswith("RUSTFLAGS")]:
        del env[key]
    env.update(RUSTUP_TOOLCHAIN=name, RA_JIT_HOME=home, RA_JIT_CACHE=os.path.join(work, "store"),
               PATH=os.path.dirname(os.path.abspath(RA_JIT)) + os.pathsep + env["PATH"],
               CARGO_TERM_COLOR="never", RUST_BACKTRACE="0", CARGO_NET_OFFLINE="true")
    if os.environ.get("RA_JIT_BACKEND"):
        env["RA_JIT_BACKEND"] = os.environ["RA_JIT_BACKEND"]
    found, failed = {}, []
    try:
        for scenario, steps in SCENARIOS:
            began = len(open(log).read().splitlines())
            for step, args, edits, wait in steps:
                apply(ws, edits)
                ran = run(["cargo", "ra-jit", *args], cwd=ws, env=env, timeout=1200)
                if wait and ran.returncode == 0:
                    for _ in range(600):
                        status = run(["cargo", "ra-jit", "status"], cwd=ws, env=env).stdout
                        if ": ready" in status:
                            break
                        time.sleep(1)
                settle(log)
                said = (ran.stderr.strip().splitlines() or [""])[-1][:100]
                print(f"  {scenario}: {step}: exit {ran.returncode}  {said}", flush=True)
                if scenario == "doctests" and ran.returncode != 0:
                    failed.append(f"{scenario}: {step}: {said}")
            lines = open(log).read().splitlines()[began:]
            runs, questions = counts(lines)
            found[scenario] = {"rustc": runs["rustc"], "rustdoc": runs["rustdoc"]}
            with open(os.path.join(work, f"{scenario.replace(' ', '-')}.log"), "w") as out:
                out.write("\n".join(lines) + "\n")
            print(f"{scenario}: rustc {runs['rustc']} ({questions} only asking), rustdoc {runs['rustdoc']}, "
                  f"cargo {runs['cargo']}", flush=True)
    finally:
        run(["cargo", "ra-jit", "stop"], cwd=ws, env=env)
        run(["rustup", "toolchain", "uninstall", name])
    baseline = json.load(open(BASELINE)) if os.path.exists(BASELINE) else {}
    recorded = baseline.get(PLACE)
    if record:
        baseline[PLACE] = found
        with open(BASELINE, "w") as out:
            json.dump(baseline, out, indent=1, sort_keys=True)
            out.write("\n")
        print(f"recorded the counts of {PLACE} in {BASELINE}")
    risen, fallen = [], []
    for scenario, now in found.items():
        for tool, count in now.items():
            was = (recorded or {}).get(scenario, {}).get(tool)
            if was is not None and count > was:
                risen.append(f"{scenario}: {tool} ran {count} times, {was} recorded")
            elif was is not None and count < was:
                fallen.append(f"{scenario}: {tool} ran {count} times, {was} recorded")
    if not keep:
        shutil.rmtree(work, ignore_errors=True)
    else:
        print(f"kept {work}")
    if recorded is None and not record:
        print(f"no counts recorded for {PLACE}; norustc.py --record records these:\n{json.dumps(found)}")
    for line in fallen:
        print(f"lower: {line}; norustc.py --record lowers the baseline")
    for line in risen:
        print(f"RISEN: {line}")
    for line in failed:
        print(f"FAILED: {line}")
    if risen and not record:
        print("ra-jit runs rustc more than it did: docs/no-rustc.md says why that is refused")
        sys.exit(1)
    if failed:
        print("the fixture's doctests failed: they pass under rustdoc")
        sys.exit(1)


if __name__ == "__main__":
    main()
