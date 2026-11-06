"""The release bootstrap authorizes its pinned server crates, not unrelated code.

Compiler and version subprocesses are mocked. Existing ownership checks,
cleanup after failure, and explicit caller feature policy remain enforced.
"""
import contextlib
import io
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parent))
import build_release as release


class MacroBootstrapTests(unittest.TestCase):
    def run_bootstrap(self, inherited=None, *, refused=0, crash=False, receipt=True,
                      target="x86_64-unknown-linux-gnu", version_error=None):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            compiler = root / "ra-jit"
            executable = root / release.SERVER_BIN
            calls = []

            def compile_server(argv, **kwargs):
                calls.append((argv, kwargs))
                executable.write_bytes(b"partial or complete server")
                if crash:
                    raise subprocess.CalledProcessError(1, argv, output="", stderr="bad code")
                return subprocess.CompletedProcess(
                    argv, 0, "", (f"linked server in 1s, {refused} functions in it ra-jit could not compile\n"
                                 if receipt else "no compilation receipt\n")
                )

            environment = {"PATH": os.environ.get("PATH", ""), "UNCHANGED": "yes"}
            if inherited is not None:
                environment["RUSTC_BOOTSTRAP"] = inherited
            with mock.patch.dict(os.environ, environment, clear=True), \
                 mock.patch.object(release, "run", side_effect=compile_server), \
                 mock.patch.object(release.subprocess, "run", side_effect=version_error,
                                   return_value=subprocess.CompletedProcess(
                     [], 0, "owned server 1.99.0\n", "")) as version, \
                 contextlib.redirect_stdout(io.StringIO()), \
                 contextlib.redirect_stderr(io.StringIO()):
                before = dict(os.environ)
                error = None
                try:
                    returned = release.proc_macro_server(compiler, root, target)
                    self.assertEqual(returned, executable)
                except (SystemExit, subprocess.CalledProcessError, OSError) as caught:
                    error = caught
                self.assertEqual(dict(os.environ), before)
                if version.called:
                    self.assertEqual(version.call_args.kwargs["env"].get("RUSTC_BOOTSTRAP"), inherited)
                result = (calls, error, executable.exists(), version.call_count)
            return result

    def test_missing_authorization_is_scoped_to_owned_server_crates(self):
        calls, error, exists, count = self.run_bootstrap()
        self.assertIsNone(error)
        self.assertTrue(exists)
        self.assertEqual(count, 1)
        self.assertEqual(calls[0][1]["env"]["RUSTC_BOOTSTRAP"], "proc_macro_api,proc_macro_srv")

    def test_explicit_bootstrap_denial_is_preserved(self):
        calls, _, _, _ = self.run_bootstrap("-1")
        self.assertEqual(calls[0][1]["env"]["RUSTC_BOOTSTRAP"], "-1")

    def test_explicit_crate_allowlist_is_not_broadened(self):
        calls, _, _, _ = self.run_bootstrap("only_caller_authorized")
        self.assertEqual(calls[0][1]["env"]["RUSTC_BOOTSTRAP"], "only_caller_authorized")

    def test_explicit_empty_bootstrap_setting_is_preserved(self):
        calls, _, _, _ = self.run_bootstrap("")
        self.assertEqual(calls[0][1]["env"]["RUSTC_BOOTSTRAP"], "")

    def test_missing_compiler_ownership_receipt_is_rejected(self):
        _, error, exists, count = self.run_bootstrap(receipt=False)
        self.assertIsInstance(error, SystemExit)
        self.assertFalse(exists)
        self.assertEqual(count, 0)

    def test_only_owned_compiler_is_used(self):
        calls, _, _, _ = self.run_bootstrap()
        argv, kwargs = calls[0]
        self.assertEqual(Path(argv[0]).name, "ra-jit")
        self.assertEqual(argv[1], str(release.SERVER))
        self.assertEqual(argv[4:8], ["--crate", release.SERVER_CRATE, "--crate", "proc_macro"])
        self.assertEqual(kwargs["env"]["RA_JIT_PROC_MACRO_SERVER"], "0")
        self.assertEqual(kwargs["env"]["RA_JIT_STD_FROM"], "off")
        self.assertEqual(kwargs["env"]["UNCHANGED"], "yes")

    def test_selected_target_is_passed_into_the_owned_compiler(self):
        # On a clean runner there is no cached target to infer. The exact release
        # target must reach owned Cargo metadata without asking an external rustc.
        for target in ("x86_64-unknown-linux-gnu", "aarch64-unknown-linux-gnu"):
            calls, error, exists, count = self.run_bootstrap(target=target)
            self.assertIsNone(error)
            self.assertTrue(exists)
            self.assertEqual(count, 1)
            self.assertEqual(calls[0][0][2:4], ["--target", target])

    def test_release_main_hands_exact_target_to_server(self):
        for target in ("x86_64-unknown-linux-gnu", "aarch64-unknown-linux-gnu"):
            with tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                compiler = root / "ra-jit"
                compiler.write_bytes(b"owned compiler fixture")
                out = root / "release"
                with mock.patch.object(release, "profile", return_value=(
                         root / "profile", {"crate": "fixture", "commit": "a" * 40})), \
                     mock.patch.object(release, "build", return_value=([compiler], 0)), \
                     mock.patch.object(release, "crate_hash", return_value="fixture"), \
                     mock.patch.object(release, "proc_macro_server") as server, \
                     mock.patch.object(release, "check_artifact") as artifact, \
                     mock.patch.object(sys, "argv", [
                         "build_release.py", "--target", target, "--out", str(out), "--server"
                     ]), \
                     contextlib.redirect_stdout(io.StringIO()):
                    release.main()
                server.assert_called_once_with(out / "ra-jit", out, target)
                artifact.assert_called_once_with(out, "portable")

    def test_failed_version_check_removes_unverified_server(self):
        _, error, exists, count = self.run_bootstrap(
            version_error=subprocess.CalledProcessError(42, ["server", "--version"]))
        self.assertIsInstance(error, subprocess.CalledProcessError)
        self.assertFalse(exists)
        self.assertEqual(count, 1)

    def test_unlaunchable_server_removes_unverified_image(self):
        _, error, exists, count = self.run_bootstrap(version_error=OSError("exec failed"))
        self.assertIsInstance(error, OSError)
        self.assertFalse(exists)
        self.assertEqual(count, 1)

    def test_failed_owned_compilation_still_refuses_the_artifact(self):
        _, error, exists, count = self.run_bootstrap(crash=True)
        self.assertIsInstance(error, subprocess.CalledProcessError)
        self.assertFalse(exists)
        self.assertEqual(count, 0)

    def test_partial_compiler_ownership_is_still_rejected(self):
        _, error, exists, count = self.run_bootstrap(refused=1)
        self.assertIsInstance(error, SystemExit)
        self.assertFalse(exists)
        self.assertEqual(count, 0)

    def test_explicit_existing_bootstrap_setting_is_preserved(self):
        calls, error, _, _ = self.run_bootstrap("1")
        self.assertIsNone(error)
        self.assertEqual(calls[0][1]["env"]["RUSTC_BOOTSTRAP"], "1")


if __name__ == "__main__":
    unittest.main()
