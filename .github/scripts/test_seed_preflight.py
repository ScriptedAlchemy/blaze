#!/usr/bin/env python3
"""Exercise the real seed preflight with bounded fake processes, never a Blaze daemon."""

from contextlib import nullcontext
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
from types import SimpleNamespace
import unittest
from unittest import mock


SCRIPT = Path(__file__).with_name("seed_preflight.py")
ACTION = SCRIPT.parents[1] / "actions" / "blaze" / "action.yml"
SPEC = importlib.util.spec_from_file_location("seed_preflight", SCRIPT)
assert SPEC and SPEC.loader
preflight = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = preflight
SPEC.loader.exec_module(preflight)


def action_step(name, path=ACTION):
    lines = path.read_text().splitlines()
    matches = [i for i, line in enumerate(lines) if line.strip() == f"- name: {name}"]
    if len(matches) != 1:
        raise AssertionError(f"{path} needs one {name} step")
    start = matches[0] + 1
    while start < len(lines) and not lines[start].strip().startswith("run:"):
        if lines[start].lstrip().startswith("- "):
            raise AssertionError(f"{name} has no shell body")
        start += 1
    if start == len(lines):
        raise AssertionError(f"{name} has no shell body")
    value = lines[start].strip().removeprefix("run:").strip()
    if value != "|":
        if value.startswith("'") and value.endswith("'"):
            value = value[1:-1].replace("''", "'")
        return value + "\n"
    indent = len(lines[start]) - len(lines[start].lstrip()) + 2
    body = []
    for line in lines[start + 1:]:
        if line and not line.startswith(" " * indent):
            break
        body.append(line[indent:] if line else "")
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
    def exercise(self, mode="ok", *, ignored=True, action=False, work_code=None, **control):
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
            elif work_code is not None:
                wrapper = root / "work.py"
                wrapper.write_text('''
import importlib.util, json, pathlib, sys
spec = importlib.util.spec_from_file_location("seed_preflight", sys.argv[1])
module = importlib.util.module_from_spec(spec)
sys.modules[spec.name] = module
spec.loader.exec_module(module)
given = module.load(sys.argv[2])
def work(daemon, log):
    with (pathlib.Path(given.log).parent / "trace.jsonl").open("a") as trace:
        trace.write(json.dumps({"command": "work", "pid": daemon.pid}) + "\\n")
    if int(sys.argv[3]):
        raise module.Failed("deliberate workspace failure", int(sys.argv[3]))
try:
    module.run(given, work=work)
except module.Failed as error:
    print(error, file=sys.stderr)
    sys.exit(error.returncode)
''')
                argv = ["bash", "-e", "-o", "pipefail", "-c", 'exec "$1" -I "$2" "$3" "$4" "$5"',
                        "seed-work-test", sys.executable, str(wrapper), str(SCRIPT), json.dumps(config),
                        str(work_code)]
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

    def test_workspace_failure_still_stops_the_owned_daemon_and_keeps_the_original_exit(self):
        for mode, work_code, expected in (("ok", 0, 0), ("ok", 23, 23),
                                           ("stop_failure", 23, 23), ("stop_failure", 0, 37)):
            with self.subTest(mode=mode, work_code=work_code):
                done, trace, _ = self.exercise(mode, work_code=work_code)
                self.assertEqual(done.returncode, expected, done.stdout + done.stderr)
                commands = [row["command"] for row in trace]
                self.assertLess(commands.index("work"), commands.index("stop"))
                self.assertEqual(next(row["pid"] for row in trace if row["command"] == "work"),
                                 next(row["pid"] for row in trace if row["command"] == "serve"))
                if work_code:
                    self.assertIn("deliberate workspace failure", done.stderr)

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

    def test_cancellation_during_spawn_reaps_its_owned_child(self):
        real_popen = subprocess.Popen
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            child = root / "sleeping-command"
            child.write_text("#!/bin/sh\nexec " + shlex.quote(sys.executable)
                             + " -I -c 'import time; time.sleep(60)'\n")
            child.chmod(0o700)
            given = preflight.Input(str(child), str(root / "seed.log"), cleanup_seconds=0.2)

            def exercise(deferred):
                spawned = []

                def launch(*args, **kwargs):
                    process = real_popen(*args, **kwargs)
                    spawned.append(process)
                    # The child exists, but command() has not received its handle.
                    # Only the actual deferred mask can postpone this cancellation.
                    os.kill(os.getpid(), signal.SIGTERM)
                    return process

                handler = signal.signal(signal.SIGTERM, preflight.interrupted)
                previous = signal.pthread_sigmask(signal.SIG_UNBLOCK, {signal.SIGTERM})
                control = nullcontext() if deferred else mock.patch.object(
                    preflight, "deferred_interrupts",
                    side_effect=lambda: nullcontext(signal.pthread_sigmask(signal.SIG_BLOCK, set())))
                try:
                    with control, mock.patch.object(preflight.subprocess, "Popen", side_effect=launch):
                        with self.assertRaises(preflight.Failed) as caught:
                            preflight.command(given, ["status", "--json"], 1.0, io.StringIO())
                    self.assertEqual(caught.exception.returncode, 128 + signal.SIGTERM)
                    self.assertEqual(len(spawned), 1)
                    process = spawned[0]
                    reaped = process.returncode is not None
                    if reaped:
                        with self.assertRaises(ChildProcessError):
                            os.waitpid(process.pid, os.WNOHANG)
                    else:
                        self.assertIsNone(preflight.exited(process), "the control must leave a live child")
                    return reaped
                finally:
                    # A regressed owner and the deliberately unmasked control must
                    # fail the oracle without leaking their real child from the test.
                    for process in spawned:
                        if process.poll() is None:
                            process.kill()
                        process.wait(timeout=2)
                    signal.pthread_sigmask(signal.SIG_SETMASK, previous)
                    signal.signal(signal.SIGTERM, handler)

            self.assertTrue(exercise(True), "command returned before reaping its cancelled child")
            self.assertFalse(exercise(False), "the oracle missed cancellation before handle ownership")


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


class ReadinessDeadlineTests(unittest.TestCase):
    def given(self, **overrides):
        values = {"blaze": "/bin/blaze", "log": "/tmp/seed.log",
                  "ready_seconds": 2.0, "status_seconds": 0.5}
        values.update(overrides)
        return preflight.Input(**values)

    def raise_ready(self, clock, command, expected=preflight.Failed, **overrides):
        process = SimpleNamespace(pid=123)
        log = io.StringIO()
        with (
            mock.patch.object(preflight, "alive"),
            mock.patch.object(preflight.time, "monotonic", side_effect=list(clock)),
            mock.patch.object(preflight, "command", side_effect=command) as status,
            self.assertRaises(expected) as caught,
        ):
            preflight.ready(self.given(**overrides), process, log)
        return caught.exception, status, log

    def test_clamped_status_timeout_reports_readiness_and_keeps_the_command_timeout(self):
        cases = (
            {"ready_seconds": 2.0, "status_seconds": 0.5,
             "clock": [10, 11.99370903, 12.01], "within": "2"},
            {"ready_seconds": 120.0, "status_seconds": 120.0,
             "clock": [0, 119.99370903, 120.01], "within": "120"},
        )
        detail = "blaze status --json did not finish within 0.00629097 seconds"
        for case in cases:
            with self.subTest(ready_seconds=case["ready_seconds"]):
                failure = preflight.CommandTimeout(detail)

                def command(_given, args, seconds, log, failure=failure, case=case):
                    self.assertEqual(args, ["status", "--json"])
                    self.assertLess(seconds, case["status_seconds"])
                    self.assertEqual(format(seconds, "g"), "0.00629097")
                    print("cleanup: signal 15 to owned process group 123", file=log, flush=True)
                    raise failure

                error, status, log = self.raise_ready(case["clock"], command,
                                                       ready_seconds=case["ready_seconds"],
                                                       status_seconds=case["status_seconds"])
                self.assertEqual(
                    str(error),
                    "the owned seed daemon 123 did not answer status within "
                    f"{case['within']} seconds; {detail}")
                self.assertEqual(error.returncode, 124)
                self.assertIs(error.__cause__, failure)
                self.assertEqual(status.call_count, 1)
                self.assertIn("cleanup: signal 15 to owned process group 123", log.getvalue())

    def test_status_timeout_inside_the_readiness_budget_stays_the_command_failure(self):
        # The third clock sample is past the readiness deadline, as cleanup can be.
        failure = preflight.CommandTimeout("blaze status --json did not finish within 0.5 seconds")

        def command(_given, args, seconds, log, failure=failure):
            self.assertEqual(args, ["status", "--json"])
            self.assertEqual(seconds, 0.5)
            print("cleanup: signal 9 to owned process group 123", file=log, flush=True)
            raise failure

        error, status, log = self.raise_ready([10, 11, 100], command)
        self.assertIs(error, failure)
        self.assertEqual(status.call_count, 1)
        self.assertIn("cleanup: signal 9 to owned process group 123", log.getvalue())

    def test_clamped_budget_does_not_replace_other_status_failures(self):
        failures = (
            preflight.Failed("blaze status --json exited with status 37", 37),
            preflight.Failed("blaze status --json needed forced process-group cleanup"),
            preflight.Failed("owned process group 123 still has live members [123] "
                             "after SIGKILL within 0.2 seconds", 124),
            OSError("status spawn failed"),
        )
        for failure in failures:
            with self.subTest(failure=str(failure)):
                def command(*_args, failure=failure):
                    raise failure

                error, status, _log = self.raise_ready([10, 11.99370903, 12.01], command,
                                                        type(failure))
                self.assertIs(error, failure)
                self.assertEqual(status.call_count, 1)

    def test_malformed_status_json_is_not_a_readiness_timeout(self):
        def command(_given, args, seconds, _log):
            self.assertEqual(args, ["status", "--json"])
            self.assertLess(seconds, 0.5)
            return "not-json"

        error, status, _log = self.raise_ready([10, 11.99370903, 12.01], command)
        self.assertEqual(status.call_count, 1)
        self.assertIn("did not return JSON", str(error))
        self.assertNotIn("did not answer status", str(error))

    def test_expired_readiness_budget_does_not_start_status(self):
        def command(*_args):
            raise AssertionError("status started after the readiness deadline")

        error, status, _log = self.raise_ready([10, 12], command)
        status.assert_not_called()
        self.assertEqual(error.returncode, 124)
        self.assertEqual(str(error),
                         "the owned seed daemon 123 did not answer status within 2 seconds")

    def test_owned_status_answer_is_ready(self):
        def command(_given, args, seconds, _log):
            self.assertEqual(args, ["status", "--json"])
            self.assertEqual(seconds, 0.5)
            return json.dumps({"daemon": {"pid": 123}})

        process = SimpleNamespace(pid=123)
        log = io.StringIO()
        with mock.patch.object(preflight, "alive"), \
                mock.patch.object(preflight.time, "monotonic", side_effect=[10, 11]), \
                mock.patch.object(preflight, "command", side_effect=command):
            preflight.ready(self.given(), process, log)
        self.assertIn("ready: owned daemon 123 answered status", log.getvalue())


class WorkspaceScopeTests(unittest.TestCase):
    unit = "blaze-workspace-test.scope"
    group = Path("/sys/fs/cgroup/user.slice/user-1001.slice/user@1001.service/app.slice") / unit
    origin = (preflight.MemoryLimit("/sys/fs/cgroup/system.slice/runner.service", "max", "max", "max"),
              preflight.MemoryLimit("/sys/fs/cgroup/system.slice", "max", "max", "max"))

    def scope(self, **changes):
        fields = dict(unit=self.unit, memory_gb="6", command=(sys.executable, "-c", "pass"),
                      entered=True, origin=self.origin, memlock=(-1, -1))
        fields.update(changes)
        return preflight.WorkspaceScope(**fields)

    def properties(self, **changes):
        fields = dict(Id=self.unit, ActiveState="active", LoadState="loaded", InvocationID="a" * 32,
                      ControlGroup="/" + str(self.group.relative_to(preflight.CGROUPS)))
        fields.update(changes)
        return fields

    def limits(self):
        return (preflight.MemoryLimit(str(self.group), str(6 << 30), "max", "max"),
                preflight.MemoryLimit(str(self.group.parent), str(3 << 30), str(2 << 30), "0"))

    def test_effective_target_limits_remain_stricter_than_the_configured_cap(self):
        proof = preflight.scope_proof(self.scope(), self.properties(), self.group, self.limits(), (-1, -1))
        self.assertEqual(proof["effective_limits"],
                         {"memory.max": 3 << 30, "memory.high": 2 << 30, "memory.swap.max": 0})
        self.assertEqual(proof["configured_memory_bytes"], 6 << 30)

    def test_a_scope_cannot_leave_any_finite_source_ancestor_constraint(self):
        for field in ("maximum", "high", "swap"):
            with self.subTest(field=field):
                fields = dict(path=self.origin[0].path, maximum="max", high="max", swap="max")
                fields[field] = str(1 << 30)
                rows = (preflight.MemoryLimit(**fields),)
                with self.assertRaisesRegex(preflight.Failed, "would leave"):
                    preflight.preserve_ancestors(rows, self.group)
                # Remaining below that ancestor keeps its aggregate constraint.
                preflight.preserve_ancestors(rows, Path(rows[0].path) / "owned.scope")

    def test_missing_nonroot_controls_cannot_truncate_ancestor_proof(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            group = root / "parent" / "owned.scope"
            group.mkdir(parents=True)
            for path in (group, group.parent):
                for name in ("memory.max", "memory.high", "memory.swap.max"):
                    (path / name).write_text("max\n")
            self.assertEqual(len(preflight.memory_limits(group, root)), 2)
            (group.parent / "memory.high").unlink()
            with self.assertRaises(FileNotFoundError):
                preflight.memory_limits(group, root)

    def test_bad_membership_cap_invocation_or_memlock_stops_before_daemon_start(self):
        cases = (
            (self.group.parent / "foreign.scope", self.properties(), self.limits(), (-1, -1)),
            (self.group, self.properties(InvocationID=""), self.limits(), (-1, -1)),
            (self.group, self.properties(),
             (preflight.MemoryLimit(str(self.group), str(7 << 30), "max", "max"),), (-1, -1)),
            (self.group, self.properties(), self.limits(), (8192, 8192)),
        )
        for group, properties, limits, memlock in cases:
            with self.subTest(group=group, properties=properties, limits=limits, memlock=memlock), \
                    tempfile.TemporaryDirectory() as directory:
                given = preflight.Input("/bin/blaze", str(Path(directory) / "serve.log"), scope=self.scope())
                with (mock.patch.dict(os.environ, {"BLAZE_DAEMON_MEMORY_GB": "6"}),
                      mock.patch.object(preflight, "process_cgroup", return_value=group),
                      mock.patch.object(preflight, "unit_properties", return_value=properties),
                      mock.patch.object(preflight, "memory_limits", return_value=limits),
                      mock.patch.object(preflight.resource, "getrlimit", return_value=memlock),
                      mock.patch.object(preflight, "run") as daemon,
                      self.assertRaises(preflight.Failed)):
                    preflight.entered_scope(given)
                daemon.assert_not_called()

    def test_an_outside_daemon_prevents_scope_creation_without_being_stopped(self):
        with tempfile.TemporaryDirectory() as directory:
            scope = self.scope(entered=False, origin=(), memlock=None)
            given = preflight.Input("/bin/blaze", str(Path(directory) / "serve.log"), scope=scope)
            with (mock.patch.dict(os.environ, {"BLAZE_DAEMON_MEMORY_GB": "6"}),
                  mock.patch.object(preflight, "process_cgroup", return_value=Path(self.origin[0].path)),
                  mock.patch.object(preflight, "memory_limits", return_value=self.origin),
                  mock.patch.object(preflight, "systemd", return_value={"ControlGroup": "/user.slice/app.slice"}),
                  mock.patch.object(preflight, "command", return_value='{"daemon":{"pid":123}}') as command,
                  mock.patch.object(preflight.subprocess, "Popen") as launch,
                  self.assertRaisesRegex(preflight.Failed, "outside workspace daemon")):
                preflight.launch_scope(given)
            launch.assert_not_called()
            self.assertEqual(command.call_args.args[1], ["status", "--json"])

    def test_cancellation_during_scope_cleanup_preserves_the_first_failure(self):
        for status in (23, 0):
            with self.subTest(status=status), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                scope = self.scope(entered=False, origin=(), memlock=None)
                given = preflight.Input("/bin/blaze", str(root / "serve.log"), scope=scope)
                process = SimpleNamespace(pid=321)
                finished = []

                def completed(owned, _seconds):
                    self.assertIs(owned, process)
                    if status == 0:
                        # Scope identity/cap validation has its own tests. This
                        # fixture isolates cancellation after the workload's exit.
                        (root / "workspace-scope.json").write_text("{}")
                    return status

                def clean_scope(_given, owned, _log):
                    self.assertIs(owned, process)
                    os.kill(os.getpid(), signal.SIGTERM)
                    finished.append("scope")
                    return False

                def cleanup(owned, _seconds, _log):
                    self.assertIs(owned, process)
                    finished.append("process")
                    return False

                handler = signal.signal(signal.SIGTERM, preflight.interrupted)
                previous = signal.pthread_sigmask(signal.SIG_UNBLOCK, {signal.SIGTERM})
                try:
                    with (mock.patch.dict(os.environ, {"BLAZE_DAEMON_MEMORY_GB": "6"}),
                          mock.patch.object(preflight, "process_cgroup", return_value=Path(self.origin[0].path)),
                          mock.patch.object(preflight, "memory_limits", return_value=self.origin),
                          mock.patch.object(preflight.resource, "getrlimit", return_value=(-1, -1)),
                          mock.patch.object(preflight, "systemd", return_value={"ControlGroup": "/user.slice/app.slice"}),
                          mock.patch.object(preflight, "command", return_value='{"daemon":null}'),
                          mock.patch.object(preflight.subprocess, "Popen", return_value=process),
                          mock.patch.object(preflight, "wait_exit", side_effect=completed),
                          mock.patch.object(preflight, "clean_scope", side_effect=clean_scope),
                          mock.patch.object(preflight, "cleanup", side_effect=cleanup),
                          self.assertRaises(preflight.Failed) as caught):
                        preflight.launch_scope(given)
                finally:
                    signal.pthread_sigmask(signal.SIG_SETMASK, previous)
                    signal.signal(signal.SIGTERM, handler)
                self.assertEqual(finished, ["scope", "process"])
                self.assertEqual(caught.exception.returncode, status or 128 + signal.SIGTERM)

    def test_cleanup_needs_the_matching_proved_invocation_and_never_touches_a_foreign_unit(self):
        with tempfile.TemporaryDirectory() as directory:
            given = preflight.Input("/bin/blaze", str(Path(directory) / "serve.log"), scope=self.scope())
            process = SimpleNamespace(pid=321)
            with mock.patch.object(preflight, "unit_properties") as query:
                self.assertFalse(preflight.clean_scope(given, process, io.StringIO()))
            query.assert_not_called()  # Failed creation has no unit-cleanup authority.
            preflight.evidence(given, "workspace-scope.json",
                               {"unit": self.unit, "supervisor_pid": 321,
                                "invocation_id": "a" * 32, "cgroup": str(self.group)})
            with (mock.patch.object(preflight, "unit_properties",
                                    return_value=self.properties(InvocationID="b" * 32)),
                  mock.patch.object(preflight, "systemd") as signal_unit,
                  self.assertRaisesRegex(preflight.Failed, "different invocation")):
                preflight.clean_scope(given, process, io.StringIO())
            signal_unit.assert_not_called()
            with mock.patch.object(preflight, "unit_properties",
                                   return_value=self.properties(ActiveState="inactive", ControlGroup="")):
                self.assertFalse(preflight.clean_scope(given, process, io.StringIO()))

    def test_scope_input_keeps_argv_literal_and_refuses_unknown_or_unbounded_configuration(self):
        raw = {"blaze": "/bin/blaze", "log": "/tmp/serve.log",
               "scope": {"unit": self.unit, "memory_gb": "6", "command": ["/bin/echo", "$literal"]}}
        given = preflight.load(json.dumps(raw))
        self.assertEqual(given.scope.command, ("/bin/echo", "$literal"))
        self.assertEqual(given.scope.memory_bytes, 6 << 30)
        for changes in ({"unknown": 1}, {"seconds": 0}, {"entered": True}, {"memory_gb": "NaN"},
                        {"command": ["relative"]}, {"unit": "foreign.scope"}):
            with self.subTest(changes=changes), self.assertRaises((TypeError, ValueError)):
                preflight.load(json.dumps({**raw, "scope": {**raw["scope"], **changes}}))


class CompilerWorkflowWiringTests(unittest.TestCase):
    def fixture(self, root, *, fail="", code=0):
        (root / "bin").mkdir()
        (root / "pins").mkdir()
        (root / "pins/toolchain").write_text("nightly-fixture\n")
        program = root / "command.py"
        program.write_text('''
import json, os, pathlib, sys
name, *args = sys.argv[1:]
with pathlib.Path(os.environ["TRACE"]).open("a") as out:
    out.write(json.dumps([name, *args]) + "\\n")
sys.exit(int(os.environ["FAIL_CODE"]) if name == os.environ["FAIL_STAGE"] else 0)
''')
        for name in ("cargo", "ci_test", "ci_storage", "git", "build.sh"):
            exe = root / "bin" / name
            exe.write_text("#!/bin/sh\nexec " + shlex.quote(sys.executable) + " -I "
                           + shlex.quote(str(program)) + " " + shlex.quote(name) + ' "$@"\n')
            exe.chmod(0o700)
        env = dict(os.environ, PATH=str(root / "bin") + os.pathsep + os.environ.get("PATH", ""),
                   BLAZE_SCRIPTS=str(root / "bin"), TRACE=str(root / "trace.jsonl"),
                   FAIL_STAGE=fail, FAIL_CODE=str(code), GITHUB_ACTION_PATH=str(root / "bin"),
                   SCRIPT_DIR=str(root / "scripts with spaces"), SCRIPT_KEY="literal-$key",
                   RUNNER_TEMP=str(root / 'runner temp-$literal"'),
                   CARGO_TARGET_DIR=str(root / 'runner temp-$literal"/blaze-fs/target'),
                   HOME=str(root / "workspace home"), CARGO_HOME=str(root / "cargo cache"),
                   GITHUB_ENV=str(root / "github-env"))
        return env

    def trace(self, root):
        return [json.loads(line) for line in (root / "trace.jsonl").read_text().splitlines()]

    def test_workspace_commands_keep_locked_all_targets_and_propagate_failures(self):
        workflow = SCRIPT.parents[1] / "workflows/blaze.yaml"
        commands = (
            ("cargo check --workspace --exclude blaze-std --all-targets",
             ["cargo", "+nightly-fixture", "check", "--locked", "--workspace", "--exclude",
              "blaze-std", "--all-targets", "--message-format", "short"]),
            ("cargo test --workspace --exclude blaze-std", ["ci_test"]),
        )
        for name, wanted in commands:
            body = action_step(name, workflow)
            for code in (0, 23):
                with self.subTest(name=name, code=code), tempfile.TemporaryDirectory() as directory:
                    root = Path(directory)
                    env = self.fixture(root, fail=wanted[0], code=code)
                    done = subprocess.run(["bash", "-e", "-o", "pipefail", "-c", body], cwd=root,
                                          env=env, text=True, capture_output=True, timeout=15)
                    self.assertEqual(done.returncode, code, done.stdout + done.stderr)
                    trace = self.trace(root)
                    if wanted[0] == "ci_test":
                        self.assertEqual(len(trace), 1)
                        self.assertEqual(trace[0][0], "ci_test")
                        self.assertEqual(len(trace[0]), 2)
                        self.assertEqual(json.loads(trace[0][1]), {
                            "exclude": ["blaze-std"],
                            "evidence": env["RUNNER_TEMP"] + "/workspace-storage-evidence",
                        })
                    else:
                        self.assertEqual(trace, [wanted])

    def test_workspace_profile_changes_only_debug_metadata_after_the_shelf(self):
        workflow = SCRIPT.parents[1] / "workflows/blaze.yaml"
        name = "workspace test builds without debug metadata"
        body = action_step(name, workflow)
        cargo_job = workflow.read_text().split("\n  cargo:\n", 1)[1].split("\n  distributed-core:\n", 1)[0]
        self.assertLess(cargo_job.index("uses: ./.github/actions/toolchain-shelf"), cargo_job.index(name))
        self.assertLess(cargo_job.index(name), cargo_job.index("native launcher, constructors, TLS and cleanup"))
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            env = self.fixture(root)
            done = subprocess.run(["bash", "-e", "-o", "pipefail", "-c", body], cwd=root,
                                  env=env, text=True, capture_output=True, timeout=15)
            self.assertEqual(done.returncode, 0, done.stdout + done.stderr)
            self.assertEqual(Path(env["GITHUB_ENV"]).read_text().splitlines(), [
                "CARGO_PROFILE_DEV_DEBUG=0", "CARGO_PROFILE_TEST_DEBUG=0"])
            self.assertEqual(self.trace(root), [["git", "rev-parse", "HEAD", "HEAD^{tree}"],
                                                ["git", "status", "--short"],
                                                ["git", "diff", "HEAD", "--binary"]])

    def test_workspace_storage_passes_explicit_paths_and_keeps_failure_evidence(self):
        workflow = SCRIPT.parents[1] / "workflows/blaze.yaml"
        source = workflow.read_text()
        before = "storage before workspace compiler tests"
        after = "storage after workspace compiler tests"
        self.assertLess(source.index(before), source.index("native launcher, constructors, TLS and cleanup"))
        self.assertLess(source.index("name: cargo test --workspace --exclude blaze-std"), source.index(after))
        self.assertIn("if: ${{ always() && env.BLAZE_SCRIPTS != '' }}",
                      source.split("- name: " + after, 1)[1].split("run:", 1)[0])
        artifact = source.split("- name: keep workspace storage and partial build evidence", 1)[1].split("\n      - ", 1)[0]
        self.assertIn("if: always()", artifact)
        self.assertIn("name: workspace-storage-evidence", artifact)
        for name, phase in ((before, "before-tests"), (after, "after-tests")):
            for code in (0, 23):
                with self.subTest(name=name, code=code), tempfile.TemporaryDirectory() as directory:
                    root = Path(directory)
                    env = self.fixture(root, fail="ci_storage", code=code)
                    done = subprocess.run(["bash", "-e", "-o", "pipefail", "-c", action_step(name, workflow)],
                                          cwd=root, env=env, text=True, capture_output=True, timeout=15)
                    self.assertEqual(done.returncode, code, done.stdout + done.stderr)
                    trace = self.trace(root)
                    self.assertEqual(len(trace), 1)
                    self.assertEqual(trace[0][0], "ci_storage")
                    self.assertEqual(len(trace[0]), 2)
                    self.assertEqual(json.loads(trace[0][1]), {
                        "volume": env["RUNNER_TEMP"] + "/blaze-fs",
                        "target": env["CARGO_TARGET_DIR"],
                        "home": env["HOME"],
                        "cargo_home": env["CARGO_HOME"],
                        "output": env["RUNNER_TEMP"] + "/workspace-storage-evidence",
                        "phase": phase,
                    })

    def test_script_cache_miss_preserves_build_argv_and_failure(self):
        action = SCRIPT.parents[1] / "actions/script/action.yml"
        body = action_step("build missing scripts", action)
        body = body.replace("${{ steps.key.outputs.dir }}", "$SCRIPT_DIR")
        body = body.replace("${{ steps.key.outputs.key }}", "$SCRIPT_KEY")
        for code in (0, 37):
            with self.subTest(code=code), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                env = self.fixture(root, fail="build.sh", code=code)
                done = subprocess.run(["bash", "-e", "-o", "pipefail", "-c", body], cwd=root,
                                      env=env, text=True, capture_output=True, timeout=15)
                self.assertEqual(done.returncode, code, done.stdout + done.stderr)
                self.assertEqual(self.trace(root), [["build.sh", env["SCRIPT_DIR"], env["SCRIPT_KEY"]]])


if __name__ == "__main__":
    unittest.main()
