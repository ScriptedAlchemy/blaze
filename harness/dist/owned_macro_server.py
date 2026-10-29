#!/usr/bin/env python3
"""Build the matching proc-macro server executable through blaze, for source_macro.py.

The server's executable is compiled and linked by blaze, including source proc_macro.
Block every bootstrap compiler/Cargo call, without hiding compilation lines.
Never use the toolchain server as the owned server.
Run under the heavy lock and memory watchdog.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import time
import uuid

HARNESS = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(HARNESS))
import norustc
import tracedecay

SERVER = HARNESS.parent / "vendor" / "proc-macro-srv"


def runtime_record(server, compiler):
    server, compiler = Path(server).resolve(strict=True), Path(compiler).resolve(strict=True)
    provenance = json.loads((server.parent / "image-pins.json").read_text())
    digest = hashlib.sha256(server.read_bytes()).hexdigest()
    assert provenance["server"]["sha256"] == digest, "owned runtime differs from its bootstrap receipt"
    compiler_digest = hashlib.sha256(compiler.read_bytes()).hexdigest()
    return {"path": str(server), "sha256": digest, "bootstrap": provenance,
            "compiler_sha256": compiler_digest,
            "bootstrap_compiler_matches": provenance["blaze"]["sha256"] == compiler_digest}


def guarded_run(command, trace, *, cwd, env, blaze, memory_mib, timeout):
    label = "owned-bootstrap-" + uuid.uuid4().hex
    env = dict(env, RA_JIT_RUN_ID=label)
    argv = list(map(str, command))
    if sys.platform == "linux":
        argv = ["strace", "-f", "-D", "-e", "trace=execve", "-o", str(trace), *argv]
    stdout, stderr = Path(str(trace) + ".stdout"), Path(str(trace) + ".stderr")
    started = time.monotonic()
    with stdout.open("w") as out, stderr.open("w") as err:
        child = subprocess.Popen(argv, cwd=cwd, env=env, stdin=subprocess.DEVNULL,
                                 stdout=out, stderr=err, start_new_session=True)
        watch = tracedecay.Watchdog(str(blaze.parent), memory_mib,
                                    tracedecay.SWAP_FREE_MIN_MB, 0.5,
                                    run_id=label, process_env=False)
        watch.client = lambda: child.pid

        def owned(rows, client_tree):
            members = {child.pid}
            for pid, _, _, _ in rows:
                try:
                    if os.getsid(pid) == child.pid:
                        members.add(pid)
                except (PermissionError, ProcessLookupError):
                    pass
            return members | client_tree | tracedecay.descendants(rows, members)

        watch.pids = owned
        watch.start()
        timed_out = False
        try:
            child.wait(timeout=timeout)
        except subprocess.TimeoutExpired:
            timed_out = True
            tracedecay.kill_pids(watch.pids(tracedecay.ps_table(env=False), {child.pid}))
            child.wait()
        finally:
            watch.stop.set()
            watch.join()
    row = {"code": child.returncode, "seconds": round(time.monotonic() - started, 3),
           "timed_out": timed_out, **watch.take()}
    Path(str(trace) + ".watchdog.json").write_text(json.dumps(row, indent=2) + "\n")
    assert not timed_out, f"owned bootstrap timed out after {timeout}s"
    assert "watchdog" not in row, row
    return subprocess.CompletedProcess(command, child.returncode,
                                       stdout.read_text(errors="replace"), stderr.read_text(errors="replace"))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--blaze", type=Path, required=True)
    parser.add_argument("--work", type=Path, required=True)
    parser.add_argument("--default-sdk", action="store_true")
    parser.add_argument("--memory-mib", type=int, default=16384,
                        help="compiler/process-tree ceiling, at most 16384 MiB")
    parser.add_argument("--timeout", type=int, default=1800)
    args = parser.parse_args()
    assert 1 < args.memory_mib <= 16384, "--memory-mib must be between 2 and 16384"
    assert args.timeout > 0, "--timeout must be positive"
    limits = {"RA_JIT_MEMORY_MAX": str(args.memory_mib) + "M",
              "RA_JIT_OWN_BASE_MEMORY": str(args.memory_mib * 3 // 4) + "M",
              "RA_JIT_SPARE": "0", "RUST_BACKTRACE": "0"}
    work = args.work.resolve()
    work.mkdir(parents=True, exist_ok=False)
    compiler = args.blaze.resolve(strict=True)
    compiler_digest = hashlib.sha256(compiler.read_bytes()).hexdigest()
    log = work / "bootstrap-tools.log"
    audit = Path(norustc.stand_ins(str(work / "audit"), str(log), block=True, block_macro_server=True))
    controls = []
    for name in (*norustc.COUNTED, "rust-analyzer-proc-macro-srv"):
        path = audit / ("libexec" if name == "rust-analyzer-proc-macro-srv" else "bin") / name
        control = subprocess.run([path, "--version"], capture_output=True)
        controls.append({"tool": name, "path": str(path), "code": control.returncode})
        assert control.returncode == 97, controls
    assert [row["tool"] for row in norustc.runs(str(log))] == [row["tool"] for row in controls], controls
    (work / "tool-controls.json").write_text(json.dumps(controls, indent=2) + "\n")
    log.write_text("")
    assert any(pid == os.getpid() for pid, _, _, _ in tracedecay.ps_table(env=False)), "process sampler is unavailable"
    assert tracedecay.footprints_mb([os.getpid()]).get(os.getpid(), 0) > 0, "footprint sampler is unavailable"
    env = {**os.environ, **limits, "PATH": str(audit / "bin") + os.pathsep + os.environ["PATH"],
           "RUSTUP_TOOLCHAIN": str(audit), "RUSTC": str(audit / "bin" / "rustc"),
           "CARGO": str(audit / "bin" / "cargo"), "RA_JIT_STD_FROM": "off",
           "RA_JIT_ANSWERS": "own", "BLAZE_ANSWERS": "own",
           "RA_JIT_OWN_RESOLVE": "1", "BLAZE_OWN_RESOLVE": "1",
           "CARGO_TARGET_DIR": str(work / "target"),
           "RA_JIT_HOME": str(work / "home"), "RA_JIT_CACHE": str(work / "store"),
           "RA_JIT_PROC_MACRO_SERVER": "0"}
    if args.default_sdk:
        env.pop("SDKROOT", None)
    executable = work / "blaze-proc-macro-srv"
    command = [str(compiler), str(SERVER), "--crate",
               "rust_analyzer_proc_macro_srv", "--crate", "proc_macro",
               "--out", str(work / "server.o"), "--exe", str(executable)]
    result = guarded_run(command, work / "bootstrap.exec", cwd=Path.cwd(), env=env,
                         blaze=compiler, memory_mib=args.memory_mib, timeout=args.timeout)
    (work / "build.log").write_text("$ " + " ".join(command) + "\n" +
                                    result.stdout + result.stderr)
    calls = norustc.runs(str(log))
    (work / "bootstrap-audit.json").write_text(json.dumps(calls, indent=2) + "\n")
    execs = norustc.external_execs((work / "bootstrap.exec").read_text()) if sys.platform == "linux" else []
    (work / "bootstrap-external-execs.json").write_text(json.dumps(execs, indent=2) + "\n")
    assert not execs, f"bootstrap attempted external compiler/Cargo execs: {execs}"
    assert result.returncode == 0, f"server build exit {result.returncode}; see {work / 'build.log'}"
    assert executable.is_file(), "blaze did not link its server"
    uncompiled = re.findall(r"^linked .* in \S+, (\d+) functions in it ra-jit could not compile$",
                            result.stderr, re.M)
    assert uncompiled and all(int(count) == 0 for count in uncompiled), uncompiled
    pins = {name: dict(path=str(path), sha256=hashlib.sha256(path.read_bytes()).hexdigest())
            for name, path in [("blaze", compiler), ("server", executable)]}
    assert pins["blaze"]["sha256"] == compiler_digest, "compiler changed during bootstrap"
    check = guarded_run([str(executable), "--version"], work / "version.exec", cwd=work,
                        env={**env, "RUST_ANALYZER_INTERNALS_DO_NOT_USE": "1"},
                        blaze=compiler, memory_mib=args.memory_mib, timeout=min(args.timeout, 120))
    assert check.returncode == 0, check.stderr
    execs += norustc.external_execs((work / "version.exec").read_text()) if sys.platform == "linux" else []
    (work / "bootstrap-external-execs.json").write_text(json.dumps(execs, indent=2) + "\n")
    assert not execs, f"server version attempted external compiler/Cargo execs: {execs}"
    calls = norustc.runs(str(log))
    (work / "bootstrap-audit.json").write_text(json.dumps(calls, indent=2) + "\n")
    assert not calls, f"bootstrap attempted blocked compiler/Cargo calls: {calls}"
    assert not log.read_text().strip(), "bootstrap left an incomplete tool audit record"
    assert hashlib.sha256(compiler.read_bytes()).hexdigest() == compiler_digest, "compiler changed during bootstrap"
    assert hashlib.sha256(executable.read_bytes()).hexdigest() == pins["server"]["sha256"], "server changed during validation"
    (work / "image-pins.json").write_text(json.dumps(pins, indent=2) + "\n")
    print(f"{executable}: {check.stdout.strip()}; zero compiler/Cargo bootstrap calls")


if __name__ == "__main__":
    main()
