"""Fast checks for the owned audit's failure and cleanup boundaries; no compiler runs."""
import argparse
import contextlib
import hashlib
import io
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parent))
import norustc


class OwnedAuditTests(unittest.TestCase):
    def setUp(self):
        self.scratch = tempfile.TemporaryDirectory(prefix="norustc-selfcheck-")
        self.addCleanup(self.scratch.cleanup)
        self.root = Path(self.scratch.name).resolve()
        self.sysroot = self.root / "sysroot"
        (self.sysroot / "bin").mkdir(parents=True)
        (self.sysroot / "lib").mkdir()
        (self.sysroot / "lib" / "library").write_text("reference library")
        self.compiler = self.root / "ra-jit"
        self.server = self.root / "blaze-proc-macro-srv"
        for path in (self.compiler, self.server):
            path.write_text("#!/bin/sh\nexit 99\n")
            path.chmod(0o755)
        digest = lambda p: hashlib.sha256(p.read_bytes()).hexdigest()
        self.pins = {"blaze": {"sha256": digest(self.compiler)},
                     "server": {"sha256": digest(self.server)}}
        (self.root / "image-pins.json").write_text(json.dumps(self.pins))
        self.args = argparse.Namespace(bin=self.compiler, owned_server=self.server,
                                       work=self.root / "audit", json=self.root / "result.json",
                                       timeout=1)

    def audit(self, *, fail_build=False, tool_attempt=False, drift=False,
              accept_error=False, unready=False, timed_out=False, wrong_diagnostic=False,
              wrapped_diagnostic=False):
        commands = []

        def product(argv, trace, *, cwd, env, timeout):
            commands.append(argv)
            self.assertEqual(argv[:2], [str(self.compiler), "cargo"])
            self.assertEqual(env["RA_JIT_PROC_MACRO_SERVER"], str(self.server))
            words = argv[2:]
            if timed_out and words == ["build"]:
                raise subprocess.TimeoutExpired(argv, 1, output="partial output", stderr="partial error")
            source = (Path(cwd) / norustc.MAIN).read_text()
            code = 101 if "_wrong" in source or "fn wrong()" in source else 0
            if accept_error:
                code = 0
            if words == ["stop"] or words == ["status"]:
                code = 0
            if fail_build and words == ["build"]:
                code = 1
            if tool_attempt and words == ["status"]:
                (self.args.work / "runs.log").write_text("truncated prohibited attempt\n")
            if drift and words == ["stop"]:
                self.server.write_text("changed")
            diagnostic = ("error[E0412]: unrelated failure\n" if wrong_diagnostic
                          else "error[E0308]: expected u32, found str\n") if code == 101 else ""
            if diagnostic and wrapped_diagnostic:
                diagnostic = "error: could not build `tally`: " + diagnostic
            return subprocess.CompletedProcess(argv, code, "owned: loading\n" if unready
                                               else "owned: ready\n", diagnostic)

        with patch.object(norustc, "real_toolchain", return_value=str(self.sysroot)), \
             patch.object(norustc, "traced_run", side_effect=product), \
             patch.object(norustc.sys, "platform", "darwin"), \
             contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(io.StringIO()):
            code = norustc.owned_audit(self.args)
        return code, json.loads(self.args.json.read_text()), commands

    def test_success_checks_errors_and_removes_only_owned_scratch(self):
        code, result, commands = self.audit()
        self.assertEqual(code, 0)
        self.assertFalse(self.args.work.exists())
        errors = [row for row in result["steps"] if row["expected"] == 101]
        self.assertEqual(len(errors), 2)
        self.assertTrue(all(row["code"] == 101 for row in errors))
        self.assertEqual(result["tool_calls"], [])
        self.assertTrue(self.compiler.exists())
        self.assertEqual(commands[-1][2:], ["stop"])

    def test_failed_build_is_not_certified_by_zero_tool_count(self):
        code, result, _ = self.audit(fail_build=True)
        self.assertEqual(code, 1)
        self.assertTrue(any("exit 1, expected 0" in error for error in result["failures"]))
        self.assertTrue((self.args.work / "summary.json").is_file())
        self.assertTrue(Path(result["steps"][0]["stderr"]).is_file())

    def test_accepting_an_intentional_error_is_a_failure(self):
        code, result, _ = self.audit(accept_error=True)
        self.assertEqual(code, 1)
        self.assertEqual(sum("exit 0, expected 101" in error for error in result["failures"]), 2)

    def test_an_unrelated_failure_does_not_satisfy_the_negative_control(self):
        code, result, _ = self.audit(wrong_diagnostic=True)
        self.assertEqual(code, 1)
        self.assertEqual(sum("missing intentional type-error" in error for error in result["failures"]), 2)

    def test_wrapped_owned_compiler_diagnostics_satisfy_the_negative_control(self):
        code, result, _ = self.audit(wrapped_diagnostic=True)
        self.assertEqual(code, 0)
        self.assertEqual(result["failures"], [])

    def test_readiness_is_required_after_successful_cold_build(self):
        code, result, _ = self.audit(unready=True)
        self.assertEqual(code, 1)
        self.assertTrue(any("daemon is not ready" in error for error in result["failures"]))

    def test_timeout_retains_partial_diagnostics_and_stops_daemon(self):
        code, result, commands = self.audit(timed_out=True)
        self.assertEqual(code, 1)
        self.assertEqual(commands[-1][2:], ["stop"])
        self.assertTrue(result["steps"][0]["timed_out"])
        self.assertEqual(Path(result["steps"][0]["stderr"]).read_text(), "partial error")

    def test_even_malformed_tool_log_fails(self):
        code, result, _ = self.audit(tool_attempt=True)
        self.assertEqual(code, 1)
        self.assertIn("prohibited tool attempts logged in runs.log", result["failures"])

    def test_runtime_drift_fails(self):
        code, result, _ = self.audit(drift=True)
        self.assertEqual(code, 1)
        self.assertTrue(any("runtime pin recheck failed" in error for error in result["failures"]))

    def test_mismatched_runtime_fails_before_creating_work(self):
        self.pins["blaze"]["sha256"] = "wrong compiler"
        (self.root / "image-pins.json").write_text(json.dumps(self.pins))
        with self.assertRaisesRegex(RuntimeError, "different compiler"):
            norustc.owned_audit(self.args)
        self.assertFalse(self.args.work.exists())

    def test_blocking_controls_make_no_binary_or_library_copies(self):
        self.args.work.mkdir()
        log = self.args.work / "runs.log"
        log.write_text("")
        with patch.object(norustc, "real_toolchain", return_value=str(self.sysroot)):
            toolchain = Path(norustc.blocking_toolchain(self.args.work, log))
        controls = json.loads((self.args.work / "tool-controls.json").read_text())
        self.assertEqual([row["code"] for row in controls], [97] * 5)
        self.assertEqual(log.read_text(), "")
        self.assertEqual(list(toolchain.rglob("*.real")), [])
        self.assertTrue((toolchain / "lib" / "library").is_symlink())

    def test_environment_keeps_runtime_and_outer_process_audit_identity(self):
        with patch.dict(os.environ, {"RA_JIT_PROC_MACRO_SERVER": "official",
                                    "RA_JIT_RUN_ID": "outer-audit", "RA_JIT_OWN_RESOLVE": "0",
                                    "RA_JIT_TEST_PREFETCH_PAUSE": "stale"}):
            env = norustc.owned_environment(self.root, self.compiler, self.server, "/absolute/audit")
        self.assertEqual(env["RA_JIT_PROC_MACRO_SERVER"], str(self.server))
        self.assertEqual(env["RUSTUP_TOOLCHAIN"], "/absolute/audit")
        self.assertEqual(env["RA_JIT_RUN_ID"], "outer-audit")
        self.assertEqual(env["RA_JIT_OWN_RESOLVE"], "1")
        self.assertNotIn("RA_JIT_TEST_PREFETCH_PAUSE", env)


    def test_execveat_attempts_are_traced_and_counted(self):
        command = norustc.exec_trace(["ra-jit", "cargo", "build"], "audit.log")
        self.assertIn("trace=execve,execveat,seccomp,prctl", command)
        trace = ('execve("/repo/ra-jit", ["ra-jit"], NULL) = 0\n'
                 '[pid 20] execveat(AT_FDCWD, "/usr/bin/rustc", [], NULL, 0) = -1 ENOENT\n'
                 '[pid 21] execveat(AT_FDCWD, "/sysroot/libexec/rust-analyzer-proc-macro-srv", [], NULL, 0) = 0\n')
        self.assertEqual(norustc.external_execs(trace),
                         ["/usr/bin/rustc", "/sysroot/libexec/rust-analyzer-proc-macro-srv"])

    def test_execveat_fd_execution_fails_closed(self):
        trace = ('execve("/repo/ra-jit", [], NULL) = 0\n'
                 '[pid 20] execveat(5, "", [], NULL, AT_EMPTY_PATH) = 0\n')
        with self.assertRaisesRegex(RuntimeError, "execveat"):
            norustc.external_execs(trace)

    def test_execveat_relative_to_unknown_fd_fails_closed(self):
        trace = ('execve("/repo/ra-jit", [], NULL) = 0\n'
                 '[pid 20] execveat(5, "custom-compiler", [], NULL, 0) = 0\n')
        with self.assertRaisesRegex(RuntimeError, "execveat"):
            norustc.external_execs(trace)

    def test_unrelated_execveat_stays_allowed(self):
        self.assertEqual(norustc.external_execs('execveat(AT_FDCWD, "/usr/bin/ls", [], NULL, 0) = 0\n'), [])


if __name__ == "__main__":
    unittest.main()
