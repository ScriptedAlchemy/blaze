#!/usr/bin/env python3
"""Exercise saved BuildSpec execution with the real owned compiler.

The reference Cargo invocation belongs to this harness. Product compiler, Cargo,
rustdoc and rustup paths are fail/count stand-ins. A source-only fixture is saved,
its original tree is removed, and its result must still match the reference from
an unrelated directory with hostile ambient compiler configuration. Corrupt
inputs and unsupported input closures must produce no advertised executable.
"""
import argparse
import json
import os
from pathlib import Path
import shlex
import shutil
import signal
import subprocess
import sys
import time
import uuid

HARNESS = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(HARNESS))
import norustc
import tracedecay


def write(root, name, contents):
    path = root / name
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(contents)
    return path


def blocked_toolchain(real, root, audit):
    toolchain = root / "toolchains" / real.name
    (toolchain / "bin").mkdir(parents=True)
    for entry in real.iterdir():
        if entry.name != "bin":
            (toolchain / entry.name).symlink_to(entry, target_is_directory=entry.is_dir())
    for name in ("rustc", "rustdoc", "cargo", "rustup"):
        path = toolchain / "bin" / name
        path.write_text("#!/bin/sh\nprintf '%s\\n' " + shlex.quote(name)
                        + " >> " + shlex.quote(str(audit)) + "\nexit 97\n")
        path.chmod(0o755)
    write(root, "settings.toml", 'version = "12"\ndefault_toolchain = ' + json.dumps(real.name) + "\n")
    return toolchain


def fixture(root):
    write(root, "Cargo.toml", '[workspace]\nmembers=["app","words"]\nresolver="2"\n[profile.dev.package.saved-words]\ndebug-assertions=false\n')
    write(root, ".cargo/config.toml", '[env]\nSAVED_SPEC_LABEL={value="config-value",force=true}\n')
    write(root, "app/Cargo.toml", '''[package]
name="saved-app"
version="0.1.0"
edition="2021"
[dependencies]
renamed={package="saved-words",path="../words",default-features=false,features=["selected"]}
''')
    write(root, "app/src/main.rs", '''#[cfg(rust_analyzer)]
compile_error!("analyzer cfg entered the saved compiler request");
fn main() {
    println!("{}|{}|{}", renamed::word(), env!("SAVED_SPEC_LABEL"),
             std::env::args().nth(1).unwrap_or_default());
}
''')
    write(root, "words/Cargo.toml", '''[package]
name="saved-words"
version="0.1.0"
edition="2021"
[features]
selected=[]
''')
    write(root, "words/src/lib.rs", '''#[cfg(not(feature="selected"))]
compile_error!("the renamed dependency lost its selected feature");
#[cfg(debug_assertions)]
compile_error!("the dependency lost its per-package debug-assertions profile");
pub fn word() -> &'static str { include_str!("../message.txt") }
''')
    write(root, "words/message.txt", "captured")
    write(root, "Cargo.lock", '''version = 4
[[package]]
name = "saved-app"
version = "0.1.0"
dependencies = ["saved-words"]
[[package]]
name = "saved-words"
version = "0.1.0"
''')


class Runner:
    def __init__(self, binary, work, memory_mib, timeout):
        self.binary, self.work = binary, work
        self.memory_mib, self.timeout = memory_mib, timeout
        self.rows = []

    def run(self, name, command, env, cwd, expected=0, contains=None, guarded=True):
        label = "saved-spec-" + uuid.uuid4().hex
        env = dict(env, RA_JIT_RUN_ID=label) if guarded else dict(env)
        started = time.monotonic()
        child = subprocess.Popen(list(map(str, command)), cwd=cwd, env=env,
                                 stdin=subprocess.DEVNULL, stdout=subprocess.PIPE,
                                 stderr=subprocess.PIPE, text=True, start_new_session=True)
        watch = None
        if guarded:
            watch = tracedecay.Watchdog(str(self.binary.parent), self.memory_mib,
                                        tracedecay.SWAP_FREE_MIN_MB, 0.5, run_id=label,
                                        process_env=False)
            watch.client = lambda: child.pid

            def owned(rows, client_tree):
                # One-shot replay owns a new POSIX session. Session membership
                # retains reparented children without inspecting any process env.
                members = {child.pid}
                for pid, _, _, _ in rows:
                    try:
                        if os.getsid(pid) == child.pid:
                            members.add(pid)
                    except ProcessLookupError:
                        pass
                return members | client_tree | tracedecay.descendants(rows, members)

            watch.pids = owned
            watch.start()
        timed_out = False
        try:
            out, err = child.communicate(timeout=self.timeout)
        except subprocess.TimeoutExpired:
            timed_out = True
            if watch:
                rows = tracedecay.ps_table(env=False)
                tracedecay.kill_pids(watch.pids(rows, {child.pid}))
            else:
                os.killpg(child.pid, signal.SIGKILL)
            out, err = child.communicate()
        finally:
            if watch:
                watch.stop.set()
                watch.join()
        row = {"case": name, "code": child.returncode,
               "seconds": round(time.monotonic() - started, 3), "timed_out": timed_out}
        if watch:
            row.update(watch.take())
        self.rows.append(row)
        write(self.work, name + ".log", "$ " + shlex.join(list(map(str, command)))
              + "\n" + out + err)
        write(self.work, "results.json", json.dumps({"rows": self.rows}, indent=2) + "\n")
        print(f"{name}: exit={child.returncode}, seconds={row['seconds']}", flush=True)
        assert not timed_out and "watchdog" not in row, row
        if expected is None:
            assert child.returncode != 0, (name, out, err)
        else:
            assert child.returncode == expected, (name, child.returncode, out, err)
        if contains:
            assert contains in out + err, (name, contains, out, err)
        return out, err


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--blaze", required=True, type=Path)
    parser.add_argument("--work", required=True, type=Path)
    parser.add_argument("--memory-mib", type=int, default=8192)
    parser.add_argument("--timeout", type=int, default=600)
    parser.add_argument("--backend", choices=("cranelift", "llvm"), default="cranelift")
    args = parser.parse_args()
    assert 0 < args.memory_mib <= tracedecay.WATCHDOG_MB
    assert args.timeout > 0
    blaze, work = args.blaze.resolve(), args.work.resolve()
    work.mkdir(parents=True, exist_ok=False)
    # A missing process sampler must fail the guard instead of silently treating
    # every command as a zero-footprint success.
    assert any(pid == os.getpid() for pid, _, _, _ in tracedecay.ps_table(env=False)), "process sampler is unavailable"
    assert tracedecay.footprints_mb([os.getpid()]).get(os.getpid(), 0) > 0, "footprint sampler is unavailable"
    real = Path(norustc.real_toolchain())
    audit = work / "product-tools.log"
    blocked = blocked_toolchain(real, work / "rustup", audit)
    original = work / "original"
    fixture(original)
    template = work / "fixture-template"
    shutil.copytree(original, template)
    env = {key: value for key, value in os.environ.items()
           if not key.startswith(("RA_JIT_", "BLAZE_", "CARGO_", "RUSTC", "RUSTDOC"))
           and key not in ("RUSTUP_TOOLCHAIN", "RUSTFLAGS", "SDKROOT", "MACOSX_DEPLOYMENT_TARGET")}
    native_path = os.environ.get("PATH", os.defpath)
    env.update(PATH=str(blocked / "bin") + os.pathsep + native_path,
               RUSTUP_HOME=str(work / "rustup"), HOME=str(work / "request-home"),
               CARGO_HOME=str(work / "request-cargo"), RA_JIT_CACHE="off",
               RA_JIT_MEMORY_MAX=str(args.memory_mib) + "M",
               RA_JIT_OWN_BASE_MEMORY=str(args.memory_mib * 3 // 4) + "M",
               RA_JIT_BACKEND=args.backend, RA_JIT_SPARE="0", RUST_BACKTRACE="0")
    Path(env["HOME"]).mkdir()
    Path(env["CARGO_HOME"]).mkdir()
    runner = Runner(blaze, work, args.memory_mib, args.timeout)
    bundle = work / "bundle"
    bundle.mkdir()
    selected = ["--manifest-path", "app/Cargo.toml", "--config", "profile.dev.debug=0"]
    oracle_env = {key: value for key, value in env.items()
                  if not key.startswith(("RA_JIT_", "BLAZE_"))}
    oracle_env.update(RUSTUP_HOME=str(real.parent.parent), RUSTUP_TOOLCHAIN=str(real),
                      PATH=str(real / "bin") + os.pathsep + native_path,
                      CARGO_TARGET_DIR=str(work / "oracle-target"), RUST_BACKTRACE="0")
    try:
        expected, _ = runner.run("reference", [real / "bin/cargo", "run", "--locked", "--offline",
            *selected, "--", "argument"], oracle_env, original, guarded=False)
        assert expected == "captured|config-value|argument\n", expected
        spec = bundle / "saved.cbor"
        saved, _ = runner.run("save", [blaze, "cargo", "spec", "--out", spec, "run", *selected,
                                       "--", "argument"], env, original)
        source = original / "app/src/main.rs"
        body = source.read_text()
        source.write_text(body.replace("renamed::word()", '"edited body"'))
        changed, _ = runner.run("dirty-body-save", [blaze, "cargo", "spec", "--out",
            bundle / "dirty.cbor", "run", *selected, "--", "argument"], env, original)
        before, after = json.loads(saved), json.loads(changed)
        assert before["spec"]["2"] == after["spec"]["2"], (before, after)
        assert before["digests"]["job_content"] != after["digests"]["job_content"], (before, after)
        source.write_text(body)
        # Prove the request describes the captured content, rather than HEAD or the
        # mutable checkout: change it, then remove that entire original tree.
        write(original, "words/message.txt", "mutated after capture")
        shutil.rmtree(original)
        hostile = work / "unrelated"
        write(hostile, ".cargo/config.toml", '[build]\nrustflags=["--cfg", "hostile"]\n')
        hostile_env = dict(env, RUSTFLAGS="-Copt-level=invalid", RA_JIT_BACKEND="invalid",
                           CARGO_PROFILE_DEV_OPT_LEVEL="invalid", SAVED_SPEC_LABEL="ambient-value")
        actual, _ = runner.run("replay-after-removal", [blaze, "cargo", "spec", "--run", spec],
                               hostile_env, hostile)
        assert actual == expected, (actual, expected)
        observed = runner.rows[-1]
        assert observed["watchdog_samples"] > 0 and observed["rajit_footprint_peak_mb"] > 0, observed
        results = list(Path(str(spec) + ".outputs").glob("replay-*/result.json"))
        assert len(results) == 1, results
        artifact = json.loads(results[0].read_text())["artifacts"][0]["executable"]
        assert Path(artifact).is_file(), artifact
        if args.backend == "llvm":
            compiler_log = Path(artifact).parent / "compiler.log"
            assert any(line.startswith("backend: llvm ")
                       for line in compiler_log.read_text().splitlines()), compiler_log
        store = Path(str(spec) + ".inputs")
        source_blob = next(path for path in store.iterdir()
                           if path.is_file() and path.read_bytes() == b"captured")
        source_blob.write_bytes(b"corrupt")
        runner.run("corrupt-source", [blaze, "cargo", "spec", "--run", spec], env, hostile,
                   expected=None, contains="corrupt")
        assert list(Path(str(spec) + ".outputs").glob("replay-*/result.json")) == results
        source_blob.write_bytes(b"captured")
        # Every one of these filenames is legal on Unix, but cannot round-trip
        # through Cargo's line-oriented dep-info. Replay owns a CBOR input trace.
        project = work / "escaped-inputs"
        shutil.copytree(template, project)
        names = ["line\nbreak.txt", "carriage\rreturn.txt", "trailing\\"]
        for index, name in enumerate(names):
            write(project, "app/" + name, str(index))
        included = ",".join("include_str!(" + json.dumps("../" + name) + ")" for name in names)
        write(project, "app/src/main.rs", 'fn main() { println!("{}|{}|{}", ' + included + '); }\n')
        expected, _ = runner.run("escaped-inputs-reference", [real / "bin/cargo", "run",
            "--locked", "--offline", *selected], oracle_env, project, guarded=False)
        assert expected == "0|1|2\n", expected
        escaped = bundle / "escaped.cbor"
        runner.run("escaped-inputs-save", [blaze, "cargo", "spec", "--out", escaped,
                   "run", *selected], env, project)
        shutil.rmtree(project)
        actual, _ = runner.run("escaped-inputs-replay", [blaze, "cargo", "spec", "--run", escaped],
                               hostile_env, hostile)
        assert actual == expected, (actual, expected)
        # Each negative owns a fresh bundle; failed compilation/admission may not
        # leave a result that a caller could treat as a completed executable.
        for name in ("check", "external-include", "native-link"):
            project = work / name
            shutil.copytree(template, project)
            if name == "external-include":
                external = write(work, "uncaptured\nexternal.txt", "outside source snapshot")
                write(project, "app/src/main.rs", 'fn main() { println!("{}", include_str!('
                      + json.dumps(str(external)) + ')); }\n')
            elif name == "native-link":
                write(project, "app/src/main.rs", '''macro_rules! native {
    () => { #[link(name="m")] unsafe extern "C" { fn cos(x: f64) -> f64; } };
}
native!();
fn main() {}
''')
            saved = bundle / (name + ".cbor")
            runner.run(name + "-save", [blaze, "cargo", "spec", "--out", saved,
                       "check" if name == "check" else "build", *selected], env, project)
            reason = {"check": "does not yet execute check units", "external-include": "outside the immutable source snapshot",
                      "native-link": "native link inputs"}[name]
            runner.run(name + "-refusal", [blaze, "cargo", "spec", "--run", saved],
                       env, hostile, expected=None, contains=reason)
            assert not list(Path(str(saved) + ".outputs").glob("replay-*/result.json")), name
        print("saved source execution matches Cargo after origin removal; corrupt/external/native/unsupported inputs refused", flush=True)
    finally:
        calls = audit.read_text().splitlines() if audit.exists() else []
        write(work, "results.json", json.dumps({"backend": args.backend, "rows": runner.rows,
              "product_tool_calls": calls}, indent=2) + "\n")
        assert not calls, f"the product invoked blocked compiler tools: {calls}"


if __name__ == "__main__":
    main()
