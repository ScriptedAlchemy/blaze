"""C4 evidence controls: planted trace loss and real, short-lived Linux children.

The native cases execute copies of /usr/bin/true under forbidden tool names; they
never execute Rust or build Blaze. Missing strace is a failure on Linux, not a
skipped positive control. Root runs these controls only in the authorized CI lane.
"""

from __future__ import annotations

import json
import os
from pathlib import Path
import shutil
import sys
import tempfile
import time
import unittest
from dataclasses import asdict, replace
from typing import cast

import exec_observer as observer


def quoted(value: str) -> str:
    return '"' + "".join(f"\\x{byte:02x}" for byte in os.fsencode(value)) + '"'


def execute(path: str) -> str:
    return f"execve({quoted(path)}, [{quoted(path)}], 0x1 /* 1 var */) = 0\n"


END = "exit_group(0) = ?\n+++ exited with 0 +++\n"


class EvidenceTests(unittest.TestCase):
    def setUp(self) -> None:
        temporary = tempfile.TemporaryDirectory(prefix="exec-evidence-")
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.python = str(Path(sys.executable).resolve())
        self.traces = {
            10: execute(self.python) + "clone(flags=CLONE_VM|SIGCHLD) = 11\n" + END,
            11: execute("/tmp/blaze") + END,
        }

    def inspect(self) -> dict[str, object]:
        for old in self.root.glob("exec.trace.*"):
            old.unlink()
        for pid, value in self.traces.items():
            (self.root / f"exec.trace.{pid}").write_text(value, encoding="utf-8")
        return observer.inspect_traces(self.root, 10, self.python)

    def test_complete_process_tree_and_interleaved_syscalls(self) -> None:
        self.traces[10] = (execute(self.python) + "clone(flags=SIGCHLD <unfinished ...>\n"
                           "--- SIGRT_8 {si_code=SI_USER} ---\n"
                           "--- SIGCHLD {si_code=CLD_EXITED} ---\n<... clone resumed>) = 11\n" + END)
        result = self.inspect()
        self.assertEqual(result["processes"], 2)
        self.assertEqual(result["creations"], 1)

    def test_the_oracle_catches_a_missing_short_lived_child(self) -> None:
        del self.traces[11]
        with self.assertRaisesRegex(observer.Failed, "descendant evidence"):
            self.inspect()

    def test_the_oracle_catches_an_unterminated_or_truncated_trace(self) -> None:
        for value in (execute("/tmp/blaze"), execute("/tmp/blaze") + END[:-1], ""):
            with self.subTest(trace=value):
                self.traces[11] = value
                with self.assertRaises(observer.Failed):
                    self.inspect()

    def test_the_oracle_catches_unknown_records_and_detachment(self) -> None:
        for value in ("+++ detached +++\n", "unrecognized record\n", "execve(0x123, 0x4, 0x5) = 0\n"):
            with self.subTest(trace=value):
                self.traces[11] = value + END
                with self.assertRaises(observer.Failed):
                    self.inspect()

    def test_the_oracle_catches_absolute_and_descriptor_compiler_executions(self) -> None:
        for tool in ("cargo", "rustc", "rustdoc", "rustc.real", "cargo.exe"):
            with self.subTest(tool=tool):
                self.traces[11] = execute("/a path/" + tool) + END
                with self.assertRaisesRegex(observer.Failed, "forbidden compiler"):
                    self.inspect()
        self.traces[11] = ('execveat(3</a path/rustc>, "", [], 0x1, AT_EMPTY_PATH) = 0\n' + END)
        with self.assertRaisesRegex(observer.Failed, "forbidden compiler"):
            self.inspect()

    def test_the_oracle_catches_an_unresolved_descriptor_execution(self) -> None:
        self.traces[11] = 'execveat(3, "", [], 0x1, AT_EMPTY_PATH) = 0\n' + END
        with self.assertRaisesRegex(observer.Failed, "descriptor path"):
            self.inspect()

    def test_thread_exec_transfer_keeps_the_executable_and_both_lifetimes(self) -> None:
        # strace switches the execing thread's output to its former group leader's
        # file. Its source implementation explicitly names both sides of that move.
        self.traces[10] = (execute(self.python) + "clone(flags=CLONE_THREAD) = 11\n"
                           "+++ superseded by execve in pid 11 +++\n"
                           "<... execve resumed>) = 0\n" + END)
        self.traces[11] = f"execve({quoted('/tmp/blaze')}, [], 0x1 <pid changed to 10 ...>\n"
        self.assertEqual(self.inspect()["processes"], 2)
        self.traces[11] = f"execve({quoted('/usr/bin/cargo')}, [], 0x1 <pid changed to 10 ...>\n"
        with self.assertRaisesRegex(observer.Failed, "forbidden compiler"):
            self.inspect()

    def test_the_oracle_catches_an_unpaired_thread_exec_transfer(self) -> None:
        self.traces[11] = f"execve({quoted('/tmp/blaze')}, [], 0x1 <pid changed to 10 ...>\n"
        with self.assertRaisesRegex(observer.Failed, "identity transfer"):
            self.inspect()

    def test_typed_input_refuses_unknown_fields_and_unsafe_output_names(self) -> None:
        value: dict[str, object] = {
            "blaze": "/tmp/blaze", "cwd": "/tmp", "home": "/tmp/fresh-home", "out": "/tmp/fresh-out",
            "commands": [{"name": "protocol", "args": ["test"]}],
        }
        self.assertEqual(observer.load(json.dumps(value)).commands[0].name, "protocol")
        for field, change in (("unknown", True), ("command_seconds", True), ("reap_seconds", 0),
                              ("blaze", "relative"), ("commands", [{"name": "../escape", "args": ["test"]}]),
                              ("commands", [{"name": "stop", "args": ["test"]}]),
                              ("commands", [{"name": "supervisor", "args": ["test"]}]),
                              ("commands", [{"name": "tracer", "args": ["test"]}]),
                              ("commands", [{"name": "ok", "args": ["test"], "unknown": True}])):
            with self.subTest(field=field), self.assertRaises(ValueError):
                observer.load(json.dumps({**value, field: change}))

    def test_command_failure_and_shutdown_failure_cannot_produce_a_success_receipt(self) -> None:
        given = observer.Input("/tmp/blaze", "/tmp", "/tmp/home", "/tmp/out",
                               (observer.Command("protocol", ("test",)),))
        receipt = self.root / "supervisor.json"
        for command, stop, expected in ((23, 0, 23), (0, 7, 7), (23, 7, 23)):
            receipt.write_text(json.dumps({"pid": 10, "complete": True, "stop": stop,
                                          "commands": [{"name": "protocol", "returncode": command}]}))
            self.assertEqual(observer.read_receipt(receipt, given), (10, expected))
        receipt.write_text(json.dumps({"pid": 10, "complete": True, "stop": 0, "commands": []}))
        with self.assertRaisesRegex(observer.Failed, "omitted"):
            observer.read_receipt(receipt, given)


FAKE_BLAZE = r'''#!PYTHON
import os
from pathlib import Path
import signal
import subprocess
import sys
import time

assert sys.argv[1] == "--ignore-pin"
home = Path(os.environ["BLAZE_HOME"])
tools = Path(__file__).parent
action = sys.argv[2]
if action == "stop":
    if (home / "stop-fails").exists():
        sys.exit(7)
    if (home / "daemon.pid").exists():
        os.kill(int((home / "daemon.pid").read_text()), signal.SIGTERM)
    sys.exit(0)
if action == "fail":
    sys.exit(23)
if action == "stop-fails":
    (home / "stop-fails").write_text("yes")
elif action == "absolute":
    subprocess.run([str(tools / "rustc")], check=True)
elif action == "descriptor":
    fd = os.open(tools / "rustdoc", os.O_RDONLY)
    os.execve(fd, ["unrelated-argv-zero"], dict(os.environ))
elif action in ("late", "leak"):
    read, write = os.pipe()
    child = os.fork()
    if child == 0:
        os.close(read)
        os.setsid()
        def stopped(_number, _frame):
            if action == "late":
                subprocess.run([str(tools / "cargo")], check=True)
                os._exit(0)
        signal.signal(signal.SIGTERM, stopped)
        (home / "daemon.pid").write_text(str(os.getpid()))
        os.write(write, b"ready")
        os.close(write)
        time.sleep(60)
        os._exit(0)
    os.close(write)
    assert os.read(read, 5) == b"ready"
    os.close(read)
elif action == "drift":
    with (tools / "blaze-engine").open("ab") as output:
        output.write(b"changed")
elif action == "swap-symlink":
    link, target = Path(sys.argv[3]), Path(sys.argv[4])
    link.unlink()
    link.symlink_to(target)
print("selected fake Blaze command completed")
'''


@unittest.skipUnless(sys.platform == "linux", "kernel execution observer is Linux-only")
class NativeObserverTests(unittest.TestCase):
    def setUp(self) -> None:
        temporary = tempfile.TemporaryDirectory(prefix="exec-native-")
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name).resolve()
        binary = self.root / "bin"
        binary.mkdir()
        python = str(Path(sys.executable).resolve())
        for name in ("blaze", "blaze-engine"):
            path = binary / name
            path.write_text(FAKE_BLAZE.replace("#!PYTHON", "#!" + python), encoding="utf-8")
            path.chmod(0o700)
        true = shutil.which("true")
        self.assertIsNotNone(true, "native controls require the runner's true executable")
        assert true is not None
        for name in ("rustc", "rustdoc", "cargo"):
            shutil.copy2(true, binary / name)
        self.given = observer.Input(str(binary / "blaze"), str(self.root), str(self.root / "home"),
                                    str(self.root / "evidence"), (observer.Command("control", ("pass",)),),
                                    command_seconds=10, stop_seconds=5, reap_seconds=1)

    def run_case(self, action: str) -> tuple[int, dict[str, object]]:
        given = replace(self.given, commands=(observer.Command("control", (action,)),))
        # Round-trip the public typed input, as the workflow invokes the helper.
        code = observer.observe(observer.load(json.dumps(asdict(given))))
        value: object = json.loads((Path(given.out) / "result.json").read_text())
        return code, cast(dict[str, object], value)

    def test_native_positive_control_covers_the_selected_command_and_stop(self) -> None:
        code, result = self.run_case("pass")
        self.assertEqual(code, 0, result)
        self.assertIs(result["passed"], True)
        self.assertTrue((Path(self.given.out) / "stop.log").exists())

    def test_the_native_oracle_catches_an_absolute_short_lived_compiler(self) -> None:
        code, result = self.run_case("absolute")
        self.assertNotEqual(code, 0)
        self.assertIn("forbidden compiler", str(result))

    def test_the_native_oracle_catches_execveat_with_an_unrelated_argv_zero(self) -> None:
        code, result = self.run_case("descriptor")
        self.assertNotEqual(code, 0)
        self.assertIn("forbidden compiler", str(result))

    def test_the_native_oracle_catches_a_detached_daemons_late_compiler(self) -> None:
        code, result = self.run_case("late")
        self.assertNotEqual(code, 0)
        self.assertIn("forbidden compiler", str(result))

    def test_the_native_oracle_catches_a_descendant_left_after_successful_stop(self) -> None:
        code, result = self.run_case("leak")
        self.assertEqual(code, 124)
        self.assertIn("descendants after Blaze stop", str(result))
        # Refusal alone is insufficient: removing strace's EXITKILL would refuse
        # but leave this deliberately detached daemon alive after the tracer died.
        pid = int((Path(self.given.home) / "daemon.pid").read_text())
        deadline = time.monotonic() + 5
        while True:
            try:
                state = Path(f"/proc/{pid}/stat").read_bytes().rsplit(b")", 1)[1].split()[0]
            except FileNotFoundError:
                break
            if state in (b"Z", b"X"):
                break
            if time.monotonic() >= deadline:
                self.fail("forced observer cleanup left the detached descendant alive")
            time.sleep(0.01)

    def test_the_native_oracle_preserves_the_selected_command_failure(self) -> None:
        code, result = self.run_case("fail")
        self.assertEqual(code, 23, result)
        self.assertIs(result["passed"], False)

    def test_the_native_oracle_catches_failed_shutdown(self) -> None:
        code, result = self.run_case("stop-fails")
        self.assertEqual(code, 7, result)

    def test_the_native_oracle_catches_compiler_identity_drift(self) -> None:
        code, result = self.run_case("drift")
        self.assertNotEqual(code, 0)
        self.assertIn("changed during", str(result))

    def test_input_symlink_retargeting_cannot_change_the_launched_compiler(self) -> None:
        alias = self.root / "alias"
        alias.mkdir()
        link = alias / "blaze"
        link.symlink_to(self.given.blaze)
        target = self.root / "bin" / "cargo"
        given = replace(self.given, blaze=str(link), commands=(observer.Command(
            "control", ("swap-symlink", str(link), str(target))),))
        code = observer.observe(observer.load(json.dumps(asdict(given))))
        self.assertEqual(code, 0, (Path(given.out) / "result.json").read_text())
        config: object = json.loads((Path(given.out) / "input.json").read_text())
        self.assertEqual(cast(dict[str, object], config)["blaze"], self.given.blaze)

    def test_existing_home_cannot_connect_to_an_unobserved_daemon(self) -> None:
        Path(self.given.home).mkdir()
        with self.assertRaisesRegex(observer.Failed, "both be new"):
            observer.observe(self.given)

    def test_a_command_that_writes_no_trace_cannot_stand_in_for_strace(self) -> None:
        true = shutil.which("true")
        assert true is not None
        code = observer.observe(replace(self.given, tracer=true))
        self.assertNotEqual(code, 0)


if __name__ == "__main__":
    unittest.main()
