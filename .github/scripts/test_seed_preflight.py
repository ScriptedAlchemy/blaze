#!/usr/bin/env python3
"""Exercise the real seed preflight with bounded fake processes, never a Blaze daemon."""

import importlib.util
import io
import json
import os
from pathlib import Path
import shlex
import signal
import subprocess
import sys
import tempfile
import unittest
from unittest import mock


SCRIPT = Path(__file__).with_name("seed_preflight.py")
ACTION = SCRIPT.parents[1] / "actions" / "blaze" / "action.yml"
SPEC = importlib.util.spec_from_file_location("seed_preflight", SCRIPT)
assert SPEC and SPEC.loader
preflight = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = preflight
SPEC.loader.exec_module(preflight)


def action_step(name):
    lines = ACTION.read_text().splitlines()
    heading = f"    - name: {name}"
    if lines.count(heading) != 1:
        raise AssertionError(f"blaze action needs one {name} step")
    start = lines.index(heading) + 1
    while start < len(lines) and lines[start] != "      run: |":
        if lines[start].startswith("    - "):
            raise AssertionError(f"{name} has no shell body")
        start += 1
    body = []
    for line in lines[start + 1:]:
        if line and not line.startswith("        "):
            break
        body.append(line[8:] if line else "")
    return "\n".join(body) + "\n"


DESCENDANT = r'''
import json, os, pathlib, signal, time
root = pathlib.Path(__file__).parent
signal.signal(signal.SIGTERM, signal.SIG_IGN)
state = pathlib.Path(f"/proc/{os.getpid()}/stat").read_text().rsplit(")", 1)[1].split()
print("descendant ignores TERM", flush=True)
(root / "descendant.pending").write_text(json.dumps({"pid": os.getpid(), "start": state[19]}))
(root / "descendant.pending").replace(root / "descendant.pid")
for _ in range(6000):
    time.sleep(0.01)
'''


DOUBLE = r'''
import json, os, pathlib, signal, subprocess, sys, time
root = pathlib.Path(__file__).parent
config = json.loads((root / "double.json").read_text())
mode = config.get("mode", "ok")
assert sys.argv[1] == "--ignore-pin", sys.argv
command = sys.argv[2]
with (root / "trace.jsonl").open("a") as trace:
    trace.write(json.dumps({"command": command, "pid": os.getpid()}) + "\n")

def wait_out():
    for _ in range(6000):
        time.sleep(0.01)
    sys.exit(99)

def remember(name):
    state = pathlib.Path(f"/proc/{os.getpid()}/stat").read_text().rsplit(")", 1)[1].split()
    (root / f"{name}.pending").write_text(json.dumps({"pid": os.getpid(), "start": state[19]}))
    (root / f"{name}.pending").replace(root / f"{name}.pid")

def descendant():
    # Same group/session as the owned leader. It deliberately survives TERM and
    # its parent's exit; it holds the command's inherited output files too.
    subprocess.Popen([sys.executable, "-I", "-u", str(root / "descendant.py")],
                     stdin=subprocess.DEVNULL)
    deadline = time.monotonic() + 5
    while not (root / "descendant.pid").exists():
        if time.monotonic() >= deadline:
            sys.exit(97)
        time.sleep(0.01)

if command == "serve":
    remember("serve")
    print("seed startup log", flush=True)
    if mode.startswith("serve_child_"):
        descendant()
    if mode in ("startup_exit", "serve_child_exit"):
        print("seed failed before readiness", flush=True)
        sys.exit(77)
    if mode != "missing_sampler":
        # Deliberately printed even without SIG_IGN: the text alone proves neither
        # inherited signal disposition nor readiness.
        print("blaze serve: no CPU samples: signal already has a handler", flush=True)
    def stopped(signum, _frame):
        print(f"seed cleanup signal {signum}", flush=True)
        if not config.get("ignore_term"):
            sys.exit(0)
    signal.signal(signal.SIGTERM, stopped)
    for _ in range(6000):
        if (root / "stop-request").exists() and mode not in ("shutdown_hang", "serve_child_shutdown_hang"):
            print("seed complete shutdown log", flush=True)
            sys.exit(79 if mode == "shutdown_exit" else 0)
        time.sleep(0.01)
    sys.exit(99)
elif command == "status":
    assert sys.argv[3:] == ["--json"]
    if mode == "status_child_exit":
        remember("status")
        descendant()
    if mode == "status_hang":
        remember("status")
        print("status entered a stuck RPC", file=sys.stderr, flush=True)
        signal.signal(signal.SIGTERM, signal.SIG_IGN)
        wait_out()
    count_file = root / "status-count"
    count = int(count_file.read_text()) + 1 if count_file.exists() else 1
    count_file.write_text(str(count))
    daemon = None
    if (mode not in ("not_ready", "startup_exit", "serve_child_exit")
            and (root / "serve.pid").exists() and count >= config.get("ready_after", 2)):
        pid = json.loads((root / "serve.pid").read_text())["pid"]
        daemon = {"pid": pid + 100000 if mode == "wrong_pid" else pid}
        (root / "ready").write_text(str(count))
    print(json.dumps({"daemon": daemon}), flush=True)
elif command == "stop":
    assert sys.argv[3:] == []
    if mode == "stop_child_hang":
        remember("stop")
        descendant()
        print("stop entered a stuck RPC with a descendant", file=sys.stderr, flush=True)
        wait_out()  # TERM exits this leader; the child still needs KILL.
    if mode == "stop_hang":
        remember("stop")
        print("stop entered a stuck RPC", file=sys.stderr, flush=True)
        signal.signal(signal.SIGTERM, signal.SIG_IGN)
        wait_out()
    if mode == "stop_failure":
        print("stop deliberately failed", file=sys.stderr, flush=True)
        sys.exit(37)
    if not (root / "ready").exists():
        # The pinned seed's actual pre-readiness stop: success, nothing to stop.
        print("no daemon is running", file=sys.stderr, flush=True)
        sys.exit(0)
    (root / "stop-request").touch()
    print("stop acknowledged", flush=True)
else:
    sys.exit(98)
'''


def reset_sampler_signal():
    # Start the test shell with a known disposition; its actual BASH_ENV trap must
    # be the only cause of the daemon's inherited SIG_IGN.
    signal.signal(signal.SIGRTMIN + 6, signal.SIG_DFL)


@unittest.skipUnless(sys.platform == "linux", "the pinned seed's signal workaround is Linux-only")
class SeedPreflightTests(unittest.TestCase):
    def exercise(self, mode="ok", *, ignored=True, action=False, **control):
        with tempfile.TemporaryDirectory(prefix="blaze seed ") as directory:
            root = Path(directory)
            (root / "bin").mkdir()
            (root / "double.json").write_text(json.dumps({"mode": mode, **control}))
            implementation = root / "double.py"
            implementation.write_text(DOUBLE)
            (root / "descendant.py").write_text(DESCENDANT)
            blaze = root / "bin" / "blaze"
            blaze.write_text("#!/bin/sh\nexec " + shlex.quote(sys.executable) + " -I -u "
                             + shlex.quote(str(implementation)) + ' "$@"\n')
            blaze.chmod(0o700)
            python = root / "bin" / "python3"
            python.write_text("#!/bin/sh\nexec " + shlex.quote(sys.executable) + ' -I "$@"\n')
            python.chmod(0o700)
            env = {key: value for key, value in os.environ.items() if key != "BASH_ENV"}
            env.update(PATH=str(root / "bin") + os.pathsep + env.get("PATH", ""),
                       RUNNER_TEMP=str(root), GITHUB_ENV=str(root / "github-env"))
            setup = action_step("SIGRTMIN+6 ignored (the pinned release's signal sampler off)")
            installed = subprocess.run(["bash", "-e", "-o", "pipefail", "-c", setup], env=env,
                                       text=True, capture_output=True, timeout=10,
                                       preexec_fn=reset_sampler_signal)
            self.assertEqual(installed.returncode, 0, installed.stdout + installed.stderr)
            if ignored:
                key, value = (root / "github-env").read_text().strip().split("=", 1)
                self.assertEqual(key, "BASH_ENV")
                env[key] = value
            config = {"blaze": str(blaze), "log": str(root / "blaze-sigcheck.log"),
                      "ready_seconds": 2.0, "status_seconds": 0.5, "stop_seconds": 0.5,
                      "reap_seconds": 0.5, "cleanup_seconds": 0.2}
            if action:
                body = action_step("the release's daemon has its sampler off")
                argv = ["bash", "-e", "-o", "pipefail", "-c", body]
            else:
                argv = ["bash", "-e", "-o", "pipefail", "-c", 'exec "$1" -I "$2" "$3"',
                        "seed-test", sys.executable, str(SCRIPT), json.dumps(config)]
            remaining = []
            try:
                done = subprocess.run(argv, env=env, cwd=SCRIPT.parents[2], text=True,
                                      capture_output=True, timeout=15, preexec_fn=reset_sampler_signal)
                trace = [json.loads(line) for line in (root / "trace.jsonl").read_text().splitlines()]
                logs = {path.name: path.read_text() for path in root.glob("*.log")}
            finally:
                # A broken cleanup must fail the test without leaving its fake processes.
                for path in root.glob("*.pid"):
                    identity = json.loads(path.read_text())
                    pid = identity["pid"]
                    try:
                        fd = os.pidfd_open(pid)
                    except ProcessLookupError:
                        continue
                    try:
                        try:
                            state = Path(f"/proc/{pid}/stat").read_text().rsplit(")", 1)[1].split()
                        except (FileNotFoundError, ProcessLookupError):
                            continue
                        # Orphan zombies have exited; neither a zombie nor a reused
                        # PID should be reported as a live leak or signaled here.
                        if state[19] != identity["start"] or state[0] in ("Z", "X"):
                            continue
                        remaining.append(pid)
                        signal.pidfd_send_signal(fd, signal.SIGKILL)
                    finally:
                        os.close(fd)
            self.assertEqual(remaining, [], f"preflight left owned processes alive: {remaining}")
            return done, trace, logs

    def test_actual_action_waits_for_its_daemon_and_keeps_shutdown_logs(self):
        # The invariant: success requires this daemon's status, both sampler proofs,
        # a successful stop command and the owned daemon's successful complete exit.
        done, trace, logs = self.exercise(action=True, ready_after=4)
        self.assertEqual(done.returncode, 0, done.stdout + done.stderr)
        commands = [item["command"] for item in trace]
        self.assertEqual(commands.count("serve"), 1)
        # Scheduling the two new processes can make the first status log before
        # serve's first line. Four replies must still precede the stop request.
        self.assertEqual([command for command in commands if command != "serve"],
                         ["status", "status", "status", "status", "stop"])
        for text in ("seed startup log", "seed complete shutdown log", "ready: owned daemon", "sampler:"):
            self.assertIn(text, done.stdout)
        self.assertIn("seed complete shutdown log", logs["blaze-sigcheck.log"])
        self.assertIn("seed preflight: owned daemon ready, sampler off, clean shutdown", done.stdout)

    def test_sampler_text_without_readiness_or_with_another_pid_never_allows_stop(self):
        for mode, code, why in (("not_ready", 124, "did not answer status"),
                                ("wrong_pid", 1, "expected owned daemon")):
            with self.subTest(mode=mode):
                done, trace, _ = self.exercise(mode)
                self.assertEqual(done.returncode, code, done.stdout + done.stderr)
                self.assertIn(why, done.stderr)
                self.assertIn("no CPU samples", done.stdout)
                self.assertIn("seed cleanup signal", done.stdout)
                self.assertNotIn("stop", [item["command"] for item in trace])
                self.assertNotIn("seed preflight: owned daemon ready, sampler off, clean shutdown", done.stdout)

    def test_startup_exit_is_preserved_and_has_the_complete_startup_log(self):
        done, trace, _ = self.exercise("startup_exit")
        self.assertEqual(done.returncode, 77, done.stdout + done.stderr)
        self.assertIn("exited with status 77 before readiness", done.stderr)
        self.assertIn("seed failed before readiness", done.stdout)
        self.assertNotIn("stop", [item["command"] for item in trace])

    def test_both_inherited_signal_and_sampler_message_are_required(self):
        for mode, ignored, why in (("ok", False, "does not ignore SIGRTMIN+6"),
                                   ("missing_sampler", True, "did not say its sampler is off")):
            with self.subTest(mode=mode):
                done, trace, _ = self.exercise(mode, ignored=ignored)
                self.assertEqual(done.returncode, 1, done.stdout + done.stderr)
                self.assertIn(why, done.stderr)
                self.assertNotIn("stop", [item["command"] for item in trace])

    def test_status_stop_and_owned_reap_each_have_a_bound(self):
        for mode, why in (("status_hang", "status --json did not finish"),
                          ("stop_hang", "stop did not finish"),
                          ("shutdown_hang", "seconds after blaze stop")):
            with self.subTest(mode=mode):
                done, _, _ = self.exercise(mode, ignore_term=True)
                self.assertEqual(done.returncode, 124, done.stdout + done.stderr)
                self.assertIn(why, done.stderr)
                self.assertIn(f"cleanup: signal {signal.SIGKILL}", done.stdout)
                if mode != "shutdown_hang":
                    self.assertIn("entered a stuck RPC", done.stdout)

    def test_cleanup_cannot_replace_a_failing_stop_or_daemon_exit(self):
        for mode, code in (("stop_failure", 37), ("shutdown_exit", 79)):
            with self.subTest(mode=mode):
                done, _, _ = self.exercise(mode, ignore_term=True)
                self.assertEqual(done.returncode, code, done.stdout + done.stderr)
                self.assertIn(f"exited with status {code}", done.stderr)
                self.assertNotIn("seed preflight: owned daemon ready, sampler off, clean shutdown", done.stdout)
        self.assertIn("seed complete shutdown log", done.stdout)

    def test_descendants_are_stopped_after_their_leader_exits_without_signaling_other_groups(self):
        # Success and failure exits both leave a real TERM-ignoring child. A
        # separate live session must survive every owned-group cleanup below.
        unrelated = subprocess.Popen(
            [sys.executable, "-I", "-c", "import time; time.sleep(60)"],
            stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
            start_new_session=True,
        )
        try:
            for mode, code, why in (
                ("serve_child_clean_exit", 1, "needed forced cleanup"),
                ("serve_child_exit", 77, "exited with status 77 before readiness"),
                ("serve_child_shutdown_hang", 124, "seconds after blaze stop"),
                ("status_child_exit", 1, "status --json needed forced process-group cleanup"),
                ("stop_child_hang", 124, "stop did not finish"),
            ):
                with self.subTest(mode=mode):
                    done, _, _ = self.exercise(mode)
                    self.assertEqual(done.returncode, code, done.stdout + done.stderr)
                    self.assertIn(why, done.stderr)
                    self.assertIn("descendant ignores TERM", done.stdout)
                    self.assertIn(f"cleanup: signal {signal.SIGKILL}", done.stdout)
                    self.assertNotIn("seed preflight: owned daemon ready, sampler off, clean shutdown",
                                     done.stdout)
                    self.assertIsNone(unrelated.poll(), "cleanup signaled a different process group")
        finally:
            if unrelated.poll() is None:
                unrelated.kill()
            unrelated.wait(timeout=2)

    def test_reaped_leader_no_longer_authorizes_signaling_its_former_group(self):
        process = subprocess.Popen([sys.executable, "-I", "-c", "pass"], start_new_session=True)
        try:
            self.assertEqual(process.wait(timeout=5), 0)
            with mock.patch.object(preflight.os, "killpg") as kill:
                with self.assertRaisesRegex(preflight.Failed, "reaped before process-group cleanup"):
                    preflight.cleanup(process, 0.2, io.StringIO())
                kill.assert_not_called()
        finally:
            if process.poll() is None:
                process.kill()
                process.wait(timeout=2)


class SeedInputTests(unittest.TestCase):
    def test_input_refuses_unknown_fields_wrong_types_and_unbounded_waits(self):
        base = {"blaze": "/bin/blaze", "log": "/tmp/seed.log"}
        for changed in ({"unknown": True}, {"blaze": None}, {"log": "relative.log"},
                        {"ready_seconds": True}, {"status_seconds": 0}, {"stop_seconds": -1},
                        {"reap_seconds": float("inf")}, {"cleanup_seconds": "2"}):
            with self.subTest(changed=changed):
                with self.assertRaises((TypeError, ValueError)):
                    preflight.load(json.dumps({**base, **changed}))
        self.assertEqual(preflight.load(json.dumps(base)).ready_seconds, 120.0)

    def test_malformed_status_cannot_claim_a_daemon(self):
        for value in ({}, [], {"daemon": True}, {"daemon": {"pid": True}},
                      {"daemon": {"pid": 0}}, {"daemon": {"pid": "1"}}):
            with self.subTest(value=value):
                with self.assertRaises(preflight.Failed):
                    preflight.status_pid(json.dumps(value))


if __name__ == "__main__":
    unittest.main()
