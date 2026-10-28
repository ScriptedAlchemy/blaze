#!/usr/bin/env python3
"""Target the edit's second-build tier-retry branch under controlled production pressure.

The tier_retry file enters the real retry branch without emitted LLVM code. The production
build, pressure handler, worker reply and publication paths run unchanged. This is not an
LLVM tier-execution test. RA_JIT_BIN names the exact binary; --work must be a new short path.
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
from pressure_recovery import source, wait_for


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
        '[package]\nname="tier_pressure_app"\nversion="0.1.0"\nedition="2021"\n[workspace]\n'
    )
    text = ws / "src/main.rs"
    text.write_text(source(0))
    pressure, hold = signals / "pressure", signals / "hold"
    pressure.write_text("normal")
    binary, obj, patch, socket = (work / name for name in ("app", "app.o", "patch.o", "sock"))
    log = work / "worker.log"
    env = dict(os.environ, RA_JIT_WORKER_MEMORY="64M", RA_JIT_SPARE="0",
               RA_JIT_TEST_PRESSURE_DIR=str(signals), RA_JIT_PRESSURE_GRACE_SECS="2",
               RA_JIT_SPLICE="0")
    fx = difftest.Fixture(str(ws), ["tier_pressure_app"], ["-p", "tier_pressure_app"])
    fx.rustc_build()
    symbols = [x for rlib in fx.rlibs + difftest.sysroot_rlibs() for x in ("--symbols", rlib)]
    link = linkline.with_output(linkline.ahead(list(fx.link_args), [str(patch), str(obj)],
                                             difftest.RA_JIT), str(binary))
    command = [difftest.RA_JIT, str(ws), "--crate", "tier_pressure_app", "--out", str(obj),
               "--patch", str(patch), "--socket", str(socket), "--link-cmd", linkline.join(link),
               "--binary", str(binary), *symbols]

    def events():
        return (signals / "events").read_text().splitlines()

    def digest():
        return {p.name: hashlib.sha256(p.read_bytes()).hexdigest() for p in (obj, patch, binary)}

    def request():
        return subprocess.Popen([difftest.RA_JIT, "sync", str(socket)],
                                stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)

    def answer(client):
        stdout, stderr = client.communicate(timeout=120)
        return client.returncode, (stdout + stderr).strip()

    def output(version):
        ran = subprocess.run([str(binary)], capture_output=True, text=True, timeout=30)
        assert ran.returncode == 0 and ran.stdout.strip() == str(sum(range(24)) + 24 * (version + 1)), ran

    child, clients = None, []
    result = {"n": 1, "test": "second-build tier branch", "emitted_llvm": False,
              "provider": "controlled production provider", "grace_seconds": 2, "steps": []}
    try:
        with log.open("w") as stream:
            child = subprocess.Popen(command, env=env, stdout=stream, stderr=stream)
        wait_for(lambda: "watching" in log.read_text() or child.poll() is not None, "startup failed")
        assert child.poll() is None, log.read_text()[-4000:]
        output(0)
        reference = subprocess.run([fx.reference], capture_output=True, text=True, timeout=30)
        assert reference.stdout.strip() == str(sum(range(24)) + 24)
        for version, persistent in ((1, False), (2, True)):
            before, mark = digest(), len(events())
            hold.write_text("tier_rebuild")
            (signals / "tier_retry").write_text("one retry")
            text.write_text(source(version))
            client = request()
            clients.append(client)
            wait_for(lambda: "checkpoint tier_rebuild" in events()[mark:], "second-build branch not reached")
            assert digest() == before, "first candidate was published before tier decision"
            assert not (signals / "tier_retry").exists(), "tier retry was not consumed"
            pressure.write_text("critical")
            wait_for(lambda: any(line.startswith("sample Critical active=0") for line in events()[mark:]),
                     "critical pressure not observed at tier retry")
            admitted = events().count("job_enter")
            hold.unlink()
            time.sleep(0.3)
            assert events().count("job_enter") == admitted, "new entry in critical second build"
            assert digest() == before, "second build partially published"
            assert client.poll() is None, "second-build pressure failed before grace"
            if persistent:
                code, reply = answer(client)
                assert code != 0 and "host memory pressure" in reply and "cargo:" not in reply, reply
                assert child.poll() is None, "tier rebuild pressure terminated worker"
                assert digest() == before, "failed tier rebuild replaced published files"
                pressure.write_text("normal")
                wait_for(lambda: events()[-1].startswith("sample Normal"), "recovery not sampled")
                client = request()
                clients.append(client)
            else:
                pressure.write_text("normal")
            code, reply = answer(client)
            assert code == 0 and reply.startswith("ok patch"), reply
            assert child.poll() is None, "recovery restarted worker"
            output(version)
            assert digest() != before, "successful retry did not publish"
            result["steps"].append({"version": version, "second_branch_reached": True,
                                    "persistent_failure": persistent, "same_pid": True,
                                    "typed_pressure_reply": persistent,
                                    "no_partial_publication": True, "recovered_patch": True})
        client = request()
        clients.append(client)
        code, reply = answer(client)
        assert code == 0 and reply.startswith("ok unchanged"), reply
        result["same_source_noop_retry"] = True
        result["worker_pid"] = child.pid
        result["timing_claim"] = "none; controlled waits and shared host"
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
