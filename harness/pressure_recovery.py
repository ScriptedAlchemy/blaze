#!/usr/bin/env python3
"""Bounded warm-daemon pressure acceptance on the production admission/patch paths.

RA_JIT_BIN names the compiler. --work must be a new directory with a short absolute path.
The explicit RA_JIT_TEST_PRESSURE_DIR hook substitutes only the pressure provider and can
hold an admitted definition. No host allocation pressure or rustc product fallback is used.
Cargo builds the differential reference once, as in the other edit harnesses.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import subprocess
import time

import difftest
import linkline


def wait_for(condition, message, timeout=120):
    deadline = time.monotonic() + timeout
    while not condition():
        if time.monotonic() >= deadline:
            raise RuntimeError(message)
        time.sleep(0.01)


def source(version):
    # Multiple changed definitions force another admission after the held job. Generics,
    # closures and function-pointer/static relocations exercise dependent compilation.
    definitions = "".join(
        f"#[inline(never)] pub fn part_{i}() -> u64 {{ generic({i}u64) + {version} }}\n"
        for i in range(24)
    )
    return (
        "fn generic<T: Into<u64>>(value: T) -> u64 { (|x: u64| x + 1)(value.into()) }\n"
        "static POINTER: fn() -> u64 = part_0;\n" + definitions
        + "fn main() { let total = POINTER() + "
        + " + ".join(f"part_{i}()" for i in range(1, 24))
        + '; println!("{total}"); }\n'
    )


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--work", type=Path, required=True)
    args = parser.parse_args()
    work = args.work.resolve()
    work.mkdir(parents=True, exist_ok=False)
    ws, signals = work / "ws", work / "s"
    (ws / "src").mkdir(parents=True)
    signals.mkdir()
    (ws / "Cargo.toml").write_text(
        '[package]\nname="pressure_app"\nversion="0.1.0"\nedition="2021"\n[workspace]\n'
    )
    text = ws / "src/main.rs"
    text.write_text(source(0))
    pressure, hold = signals / "pressure", signals / "hold"
    pressure.write_text("normal")
    binary, obj, patch, socket = (work / name for name in ("app", "app.o", "patch.o", "sock"))
    log = work / "worker.log"
    env = dict(os.environ, RA_JIT_WORKER_MEMORY="64M",
               RA_JIT_TEST_PRESSURE_DIR=str(signals), RA_JIT_PRESSURE_GRACE_SECS="2",
               RA_JIT_SPLICE="0", RA_JIT_SPARE="0")
    env.pop("RA_JIT_ALONE", None)
    # Only the compiler daemon gets the controlled provider; the oracle and CLI use native
    # OS signals and the original guarded environment.
    fx = difftest.Fixture(str(ws), ["pressure_app"], ["-p", "pressure_app"])
    fx.rustc_build()
    symbols = [x for rlib in fx.rlibs + difftest.sysroot_rlibs() for x in ("--symbols", rlib)]
    link = linkline.with_output(linkline.ahead(list(fx.link_args), [str(patch), str(obj)],
                                             difftest.RA_JIT), str(binary))
    command = [
        difftest.RA_JIT, str(ws), "--crate", "pressure_app", "--out", str(obj),
        "--patch", str(patch), "--socket", str(socket), "--link-cmd", linkline.join(link),
        "--binary", str(binary), *symbols,
    ]
    result = {"n": 1, "provider": "explicit controlled production provider",
              "soft_target": "64M", "grace_seconds": 2, "steps": []}

    def events():
        path = signals / "events"
        return path.read_text().splitlines() if path.exists() else []

    def digest():
        return {str(path.name): hashlib.sha256(path.read_bytes()).hexdigest()
                for path in (obj, patch, binary)}

    def output(version):
        ran = subprocess.run([str(binary)], capture_output=True, text=True, timeout=30)
        assert ran.returncode == 0, ran.stderr
        assert ran.stdout.strip() == str(sum(range(24)) + 24 * (version + 1)), ran.stdout

    def request():
        return subprocess.Popen([difftest.RA_JIT, "sync", str(socket)],
                                stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)

    def wait_reply(client):
        stdout, stderr = client.communicate(timeout=120)
        return client.returncode, (stdout + stderr).strip()

    child = None
    clients = []
    try:
        with log.open("w") as stream:
            child = subprocess.Popen(command, env=env, stdout=stream, stderr=stream)
        wait_for(lambda: "watching" in log.read_text() or child.poll() is not None,
                 "daemon did not become warm")
        assert child.poll() is None, log.read_text()[-4000:]
        output(0)
        reference = subprocess.run([fx.reference], capture_output=True, text=True, timeout=30)
        assert reference.stdout.strip() == str(sum(range(24)) + 24)
        pid = child.pid

        for version, persistent in ((1, False), (2, True)):
            before = digest()
            mark = len(events())
            hold.write_text("define_function")
            text.write_text(source(version))
            client = request()
            clients.append(client)
            wait_for(lambda: "checkpoint define_function" in events()[mark:],
                     "edit did not reach a held admitted definition")
            pressure.write_text("critical")
            wait_for(lambda: any(line.startswith("sample Critical active=1") for line in events()[mark:]),
                     "critical signal not published while work was admitted")
            assert digest() == before, "held edit published an object or binary"
            admitted = events().count("job_enter")
            time.sleep(0.2)
            assert events().count("job_enter") == admitted, "new compile entry under critical pressure"
            hold.unlink()
            wait_for(lambda: any(line.startswith("sample Critical active=0") for line in events()[mark:]),
                     "admitted work failed to drain")
            assert digest() == before, "partially built patch was published"
            assert client.poll() is None, "client ended before the critical grace"
            time.sleep(0.2)
            assert events().count("job_enter") == admitted, "new compile entry before relief"
            if persistent:
                code, reply = wait_reply(client)
                assert code != 0 and "host memory pressure" in reply, reply
                assert "cargo:" not in reply, reply
                assert digest() == before, "failed edit changed published files"
                assert child.poll() is None and child.pid == pid, "pressure failure lost the daemon"
                pressure.write_text("normal")
                wait_for(lambda: events()[-1].startswith("sample Normal"), "relief not sampled")
                client = request()
                clients.append(client)
            else:
                pressure.write_text("normal")
            code, reply = wait_reply(client)
            assert code == 0 and reply.startswith("ok patch"), reply
            assert child.poll() is None and child.pid == pid, "recovery restarted the daemon"
            output(version)
            assert digest() != before, "completed edit did not publish"
            result["steps"].append({
                "version": version, "persistent_failure": persistent, "same_daemon": True,
                "running_job_drained": True, "no_new_entry_critical": True,
                "no_partial_publication": True, "recovered_patch": True,
            })
        # An unchanged retry proves terminal failure was cleared without changing the request.
        client = request()
        clients.append(client)
        code, reply = wait_reply(client)
        assert code == 0 and reply.startswith("ok unchanged"), reply
        result["same_source_retry"] = True
        result["compiler_pid"] = pid
        result["matches_expected_and_initial_reference"] = True
        result["timing_claim"] = "none; controlled holds and shared host are the limiters"
        (work / "result.json").write_text(json.dumps(result, indent=2) + "\n")
        print(json.dumps(result), flush=True)
    finally:
        if hold.exists():
            hold.unlink()
        pressure.write_text("normal")
        for client in clients:
            if client.poll() is None:
                client.kill()
                client.wait()
        if child is not None and child.poll() is None:
            child.terminate()
            try:
                child.wait(timeout=10)
            except subprocess.TimeoutExpired:
                child.kill()
                child.wait()


if __name__ == "__main__":
    main()
