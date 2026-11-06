"""Copy-free CLI version negotiation and invocation-owned report receipts."""
import contextlib
import hashlib
import io
import json
import os
from pathlib import Path
import socket
import sys
import tempfile
import threading
import unittest
from unittest import mock

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
import cargo_ra_jit as harness


class NativePeer:
    def __init__(self, path, reply=None):
        self.path = str(path)
        self.reply = reply or {"Restart": {"version": "current"}}
        self.requests = []
        self.errors = []
        self.retired = threading.Event()
        self.listener = socket.socket(socket.AF_UNIX)
        self.listener.bind(self.path)
        self.listener.listen()
        self.listener.settimeout(10)
        self.thread = threading.Thread(target=self.serve, daemon=True)
        self.thread.start()

    def serve(self):
        try:
            while True:
                client, _ = self.listener.accept()
                with client, client.makefile("rb") as stream:
                    envelope = json.loads(stream.readline())
                    self.requests.append(envelope)
                    client.sendall(json.dumps(self.reply).encode() + b"\n")
                if envelope["replaces"]:
                    self.retired.set()
                    return
        except Exception as error:
            self.errors.append(error)
        finally:
            self.listener.close()
            os.unlink(self.path)

    def finish(self):
        self.thread.join(timeout=10)
        if self.thread.is_alive() or self.errors:
            raise AssertionError((self.thread.is_alive(), self.errors))


def exchange(path, envelope):
    with socket.socket(socket.AF_UNIX) as client:
        client.settimeout(10)
        client.connect(str(path))
        client.sendall(json.dumps(envelope).encode() + b"\n")
        with client.makefile("rb") as answer:
            line = answer.readline()
            rest = answer.read()
    return json.loads(line) if line else None, rest


class VersionNegotiation(unittest.TestCase):
    def test_status_keeps_the_daemon_and_replacing_client_receives_native_restart(self):
        with tempfile.TemporaryDirectory(dir="/tmp" if sys.platform == "darwin" else None) as root:
            old = Path(root, "old")
            old.mkdir()
            native = NativePeer(old / "daemon.sock")
            new = Path(root, "new")
            with harness.VersionPeer(str(old), str(new)) as peer:
                status = {"version": "current", "replaces": False, "request": "Status"}
                self.assertEqual(exchange(peer.socket, status), (native.reply, b""))
                self.assertFalse(native.retired.is_set())
                build = {"version": "current", "replaces": True, "request": {"Build": {"fixture": "value"}}}
                self.assertEqual(exchange(peer.socket, build), (native.reply, b""))
                self.assertTrue(native.retired.wait(timeout=10))
                replacement = socket.socket(socket.AF_UNIX)
                replacement.bind(peer.socket)
                self.addCleanup(replacement.close)
            self.assertTrue(Path(peer.socket).exists(), "peer cleanup unlinked the replacement")
            self.assertTrue(peer.replaced)
            self.assertEqual(peer.events, [("Status", False), ({"Build": {"fixture": "value"}}, True)])
            native.finish()
            for actual, sent in zip(native.requests, (status, build)):
                self.assertEqual(actual, {**sent, "version": "current-harness-peer"})

    def test_unexpected_daemon_reply_cannot_look_like_a_successful_replacement(self):
        with tempfile.TemporaryDirectory(dir="/tmp" if sys.platform == "darwin" else None) as root:
            old = Path(root, "old")
            old.mkdir()
            native = NativePeer(old / "daemon.sock", {"Stopping": None})
            with self.assertRaisesRegex(RuntimeError, "did not receive native Restart"):
                with harness.VersionPeer(str(old), str(Path(root, "new"))) as peer:
                    self.assertEqual(exchange(peer.socket, {"version": "current", "replaces": True,
                                                          "request": "Stop"}), (None, b""))
            native.finish()
            self.assertFalse(peer.replaced)


class ReportsAndFixtures(unittest.TestCase):
    def run_main(self, work, argv, *, failure=False):
        work.mkdir()
        fixture = work / "flags"
        fixture.mkdir()
        (fixture / "diagnostic").write_text("retained evidence")
        row = ("cold", "cargo ra-jit test", "daemon", 0, "same")
        with contextlib.ExitStack() as stack:
            stack.enter_context(mock.patch.dict(os.environ, {"RA_JIT_BACKEND": "cranelift"}))
            stack.enter_context(mock.patch.object(sys, "argv", ["cargo_ra_jit.py", "--only-checks", *argv]))
            stack.enter_context(mock.patch.object(harness.tempfile, "mkdtemp", return_value=str(work)))
            stack.enter_context(mock.patch.object(harness, "flags", return_value=([row], [], "status")))
            stack.enter_context(mock.patch.object(harness, "daemon", side_effect=RuntimeError("failed stage") if failure else None,
                                                   return_value=[("native check", True, "right")]))
            stack.enter_context(mock.patch.object(harness, "plain", return_value=[]))
            stack.enter_context(contextlib.redirect_stdout(io.StringIO()))
            if failure:
                with self.assertRaisesRegex(RuntimeError, "failed stage"):
                    harness.main()
            else:
                with self.assertRaises(SystemExit) as ended:
                    harness.main()
                self.assertEqual(ended.exception.code, 0)
        return row

    def test_default_reports_survive_success_without_fixtures_or_tracked_changes(self):
        tracked = [HERE / name for name in ("CARGO-RA_JIT.md", "cargo_ra_jit.json")]
        before = {path: hashlib.sha256(path.read_bytes()).digest() if path.exists() else None for path in tracked}
        with tempfile.TemporaryDirectory(dir="/tmp" if sys.platform == "darwin" else None) as root:
            work = Path(root, "work")
            row = self.run_main(work, [])
            self.assertFalse((work / "flags").exists())
            result = json.loads((work / "report/cargo_ra_jit.json").read_text())
            self.assertEqual(result["flags"], [list(row)])
            self.assertEqual(result["projects"], [])
            self.assertTrue((work / "report/CARGO-RA_JIT.md").exists())
        after = {path: hashlib.sha256(path.read_bytes()).digest() if path.exists() else None for path in tracked}
        self.assertEqual(before, after)

    def test_completed_receipts_and_diagnostics_survive_a_later_stage_failure(self):
        with tempfile.TemporaryDirectory(dir="/tmp" if sys.platform == "darwin" else None) as root:
            work, report = Path(root, "work"), Path(root, "receipts")
            row = self.run_main(work, ["--report-dir", str(report)], failure=True)
            self.assertTrue((work / "flags/diagnostic").is_file())
            result = json.loads((report / "cargo_ra_jit.json").read_text())
            self.assertEqual(result["flags"], [list(row)])
            self.assertEqual(result["error"], "RuntimeError: failed stage")
            self.assertIn("Result: FAIL", (report / "CARGO-RA_JIT.md").read_text())

    def test_fake_toolchain_references_the_authoritative_cargo(self):
        with tempfile.TemporaryDirectory(dir="/tmp" if sys.platform == "darwin" else None) as root:
            cargo = Path(root, "authoritative-cargo")
            cargo.write_bytes(b"sole current executable")
            directory = Path(root, "toolchain")
            with mock.patch.object(harness.subprocess, "run", return_value=mock.Mock(stdout=str(cargo) + "\n")):
                harness.fake_toolchain(str(directory), "1.98.1")
            reference = directory / "bin/cargo"
            self.assertTrue(reference.is_symlink())
            self.assertEqual(reference.resolve(), cargo.resolve())
            self.assertEqual(sorted(path.name for path in directory.rglob("*") if path.is_file() and not path.is_symlink()), ["rustc"])


if __name__ == "__main__":
    unittest.main()
