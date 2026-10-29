#!/usr/bin/env python3
"""Exec-audit integrity, including kernel-filtered tracing on Linux.

Run as `python3 -I -B harness/norustc_test.py`.
"""
import errno
import json
import os
from pathlib import Path
import platform
import shutil
import socket
import subprocess
import sys
import tempfile
import unittest

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import norustc


class ExecRecords(unittest.TestCase):
    def test_missing_exec_evidence_is_not_a_zero_call_audit(self):
        for text in ("", "101 +++ exited with 0 +++\n"):
            with self.subTest(text=text), self.assertRaises(RuntimeError):
                norustc.external_execs(text)

    def test_descendants_failed_execs_and_split_records_count(self):
        trace = '''101 execve("/tools/cc", ["cc"], 0x0) = 0
102 execve("/tools/rustc", ["rustc"], 0x0 <unfinished ...>
103 execve("/tools/cargo.real", ["cargo"], 0x0) = -1 ENOENT (No such file)
102 <... execve resumed>) = 0
[pid 104] execve("/tools/rustdoc", ["rustdoc"], 0x0) = 0
105 execve("/tools/rustup", ["rustup"], 0x0) = 0
106 execve("/tools/rust-analyzer-proc-macro-srv", [], 0x0) = 0
107 execve("/tools/rust-lld", [], 0x0) = 0
'''
        self.assertEqual(norustc.external_execs(trace), [
            "/tools/rustc", "/tools/cargo.real", "/tools/rustdoc", "/tools/rustup",
            "/tools/rust-analyzer-proc-macro-srv",
        ])

    def test_quoted_syscall_text_is_not_a_syscall_record(self):
        trace = '''101 execve("/tools/python", ["python", "prctl(PR_SET_SECCOMP, ...)", "seccomp(SECCOMP_SET_MODE_FILTER, ...)", "execve(\\"/tools/rustc\\", ...)"], 0x0) = 0
'''
        self.assertEqual(norustc.external_execs(trace), [])

    def test_tracer_setup_is_distinct_from_a_tracee_filter_change(self):
        setup = '''101 prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) = 0
101 seccomp(SECCOMP_SET_MODE_FILTER, 0, 0x1234) = 0
101 execve("/tools/python", [], 0x0) = 0
'''
        self.assertEqual(norustc.external_execs(setup), [])
        for install in (
            "102 prctl(PR_SET_SECCOMP, SECCOMP_MODE_FILTER, 0x1234) = 0\n",
            "102 prctl(0x16, 0x2, 0x1234, 0, 0) = 0\n",
            "102 prctl(0x100000016, 0x2, 0x1234, 0, 0) = 0\n",
            "102 prctl(PR_SET_SECCOMP, SECCOMP_MODE_STRICT) = -1 EINVAL (Invalid argument)\n",
            "102 seccomp(SECCOMP_SET_MODE_FILTER, 0, 0x1234 <unfinished ...>\n",
            "102 seccomp(SECCOMP_SET_MODE_STRICT, 0, NULL) = 0\n",
            "102 seccomp(0x100000001, 0, 0x1234) = 0\n",
        ):
            with self.subTest(install=install), self.assertRaises(RuntimeError):
                norustc.external_execs(setup + install)


@unittest.skipUnless(sys.platform == "linux" and shutil.which("strace"),
                     "requires Linux strace")
class LiveExecTrace(unittest.TestCase):
    def test_private_thread_names_are_not_copied_into_the_trace(self):
        with tempfile.TemporaryDirectory(prefix="exec-private-", dir="/tmp") as temporary:
            trace = Path(temporary) / "exec.log"
            program = '''import ctypes,os
libc=ctypes.CDLL(None)
name=ctypes.c_char_p(os.environ["PRIVATE_THREAD_NAME"].encode())
assert libc.prctl(15,name,0,0,0) == 0
'''
            result = subprocess.run(
                norustc.exec_trace([sys.executable, "-c", program], trace),
                env=dict(os.environ, PRIVATE_THREAD_NAME="trace-private"),
                capture_output=True, timeout=8,
            )
            self.assertEqual(result.returncode, 0, result.stderr)
            text = trace.read_text()
            self.assertNotIn("trace-private", text)
            self.assertEqual(norustc.external_execs(text), [])

    def test_native_argument_bytes_reach_the_traced_process_unchanged(self):
        with tempfile.TemporaryDirectory(prefix="exec-native-", dir="/tmp") as temporary:
            trace = Path(temporary) / "exec.log"
            command = [os.fsencode(sys.executable), b"-c",
                       b"import os,sys;sys.stdout.buffer.write(os.fsencode(sys.argv[1]))", b"\xff"]
            result = subprocess.run(norustc.exec_trace(command, trace), capture_output=True, timeout=8)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(result.stdout, b"\xff")
            self.assertEqual(norustc.external_execs(trace.read_text()), [])

    def test_exec_audit_sees_descendants_and_failed_attempts(self):
        with tempfile.TemporaryDirectory(prefix="exec-audit-", dir="/tmp") as temporary:
            root = Path(temporary)
            tools = []
            for name in (*norustc.COUNTED, "rust-analyzer-proc-macro-srv"):
                tool = root / name
                tool.write_text("#!" + shutil.which("sh") + "\nexit 97\n")
                tool.chmod(0o755)
                tools.append(str(tool))
            missing = str(root / "missing/rustc")
            descendant = '''import errno,json,subprocess,sys
codes=[subprocess.run([tool]).returncode for tool in sys.argv[1:-1]]
try:
    subprocess.run([sys.argv[-1]])
except OSError as error:
    assert error.errno == errno.ENOENT, error
else:
    raise AssertionError("missing tool unexpectedly ran")
print(json.dumps(codes))
'''
            parent = "import subprocess,sys; raise SystemExit(subprocess.run(sys.argv[1:]).returncode)"
            trace = root / "exec.log"
            command = [sys.executable, "-c", parent, sys.executable, "-c", descendant,
                       *tools, missing]
            result = subprocess.run(norustc.exec_trace(command, trace), capture_output=True,
                                    text=True, timeout=8)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(json.loads(result.stdout), [97] * 5)
            self.assertEqual(norustc.external_execs(trace.read_text()), tools + [missing])

    def test_detached_trace_preserves_a_live_child_after_request_returns(self):
        with tempfile.TemporaryDirectory(prefix="exec-daemon-", dir="/tmp") as temporary:
            root = Path(temporary)
            address = str(root / "ready.sock")
            daemon = '''import os,socket,sys
connection=socket.socket(socket.AF_UNIX)
connection.settimeout(5)
connection.connect(sys.argv[1])
connection.sendall(b"ready")
assert connection.recv(4) == b"stop"
connection.sendall(b"done")
os._exit(0)
'''
            parent = '''import subprocess,sys
subprocess.Popen(sys.argv[1:], stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL,
                 stderr=subprocess.DEVNULL, start_new_session=True)
'''
            with socket.socket(socket.AF_UNIX) as listener:
                listener.settimeout(8)
                listener.bind(address)
                listener.listen()
                trace = root / "exec.log"
                result = norustc.traced_run(
                    [sys.executable, "-c", parent, sys.executable, "-c", daemon, address],
                    trace, cwd=root, env=dict(os.environ), timeout=8,
                )
                self.assertEqual(result.returncode, 0, result.stderr)
                connection, _ = listener.accept()
                with connection:
                    connection.settimeout(5)
                    self.assertEqual(connection.recv(5), b"ready")
                    connection.sendall(b"stop")
                    self.assertEqual(connection.recv(4), b"done")
                    self.assertEqual(connection.recv(1), b"")
            self.assertEqual(norustc.external_execs(trace.read_text()), [])

    @unittest.skipUnless(platform.machine().lower() in ("x86_64", "amd64", "aarch64", "arm64"),
                         "seccomp fixture requires an x86-64 or AArch64 execve number")
    def test_a_denying_tracee_filter_cannot_produce_a_false_empty_audit(self):
        numbers = ((59, 157, 317) if platform.machine().lower() in ("x86_64", "amd64")
                   else (221, 167, 277))
        program = '''import ctypes,errno,json,os,sys
class Instruction(ctypes.Structure):
    _fields_=[("code",ctypes.c_ushort),("jt",ctypes.c_ubyte),("jf",ctypes.c_ubyte),("k",ctypes.c_uint)]
class Program(ctypes.Structure):
    _fields_=[("length",ctypes.c_ushort),("instructions",ctypes.POINTER(Instruction))]
instructions=(Instruction*4)(Instruction(0x20,0,0,0),
    Instruction(0x15,0,1,int(sys.argv[1])),Instruction(0x06,0,0,0x00050000|errno.EPERM),
    Instruction(0x06,0,0,0x7fff0000))
program=Program(4,instructions)
libc=ctypes.CDLL(None,use_errno=True)
if libc.prctl(38,1,0,0,0) != 0:
    raise OSError(ctypes.get_errno(),"cannot set subprocess-only no-new-privileges")
if sys.argv[5] == "prctl":
    installed=libc.prctl(22,2,ctypes.byref(program),0,0)
elif sys.argv[5] == "high-prctl":
    installed=libc.syscall(ctypes.c_long(int(sys.argv[3])),ctypes.c_ulong(0x100000016),
        ctypes.c_ulong(2),ctypes.byref(program),ctypes.c_ulong(0),ctypes.c_ulong(0))
else:
    installed=libc.syscall(ctypes.c_long(int(sys.argv[4])),ctypes.c_ulong(0x100000001),
        ctypes.c_ulong(0),ctypes.byref(program))
if installed != 0:
    raise OSError(ctypes.get_errno(),"cannot install subprocess-only seccomp fixture")
try:
    os.execv(sys.argv[2],[sys.argv[2]])
except OSError as error:
    assert error.errno == errno.EPERM, error
    print(json.dumps({"denied":error.errno}))
else:
    raise AssertionError("denied exec unexpectedly returned")
'''
        for method in ("prctl", "high-prctl", "high-seccomp"):
            with self.subTest(method=method), tempfile.TemporaryDirectory(
                    prefix="exec-filter-", dir="/tmp") as temporary:
                root = Path(temporary)
                tool = root / "rustc"
                tool.write_text("#!" + shutil.which("sh") + "\nexit 97\n")
                tool.chmod(0o755)
                trace = root / "exec.log"
                result = subprocess.run(
                    norustc.exec_trace([sys.executable, "-c", program, str(numbers[0]), str(tool),
                                       str(numbers[1]), str(numbers[2]), method], trace),
                    capture_output=True, text=True, timeout=8,
                )
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(json.loads(result.stdout), {"denied": errno.EPERM})
                with self.assertRaises(RuntimeError):
                    norustc.external_execs(trace.read_text())


if __name__ == "__main__":
    unittest.main()
