#!/usr/bin/env python3
"""Regression checks for both installed toolchain identities and portable CI hosts."""
import os
import subprocess
import tempfile
import unittest
from pathlib import Path

CHECK = Path(__file__).with_name("check_installed_toolchain.sh")
WORKFLOW = Path(__file__).resolve().parents[1] / "workflows" / "install.yaml"

class ToolchainIdentityTest(unittest.TestCase):
    def check(self, tmp, identity, success):
        binary = tmp / "bin"
        binary.mkdir(exist_ok=True)
        blaze = binary / "blaze"
        blaze.write_text('#!/bin/sh\nprintf "%s\\n" "$MOCK_TOOLCHAIN_OUTPUT"\n')
        blaze.chmod(0o755)
        env = dict(os.environ, PATH=f"{binary}:{os.environ['PATH']}", MOCK_TOOLCHAIN_OUTPUT=identity)
        proc = subprocess.run(["sh", str(CHECK)], env=env, capture_output=True, text=True)
        self.assertEqual(proc.returncode == 0, success, proc.stderr)

    def test_native_candidate(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            path = root / "toolchain with spaces"
            path.mkdir()
            self.check(root, f"blaze builds with Rust 1.99.0 from rustup, at {path}", True)

    def test_published_nightly(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            path = root / "nightly"
            path.mkdir()
            self.check(root, f"blaze builds with blaze-2026-10-27, blaze's toolchain (Rust nightly-2026-10-27) at {path}", True)

    def test_refuses_unknown_missing_and_relative(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            self.check(root, f"arbitrary version at {root}", False)
            self.check(root, f"blaze builds with Rust 1.99.0 from rustup, at {root / 'missing'}", False)
            self.check(root, "blaze builds with Rust 1.99.0 from rustup, at relative/toolchain", False)

    def test_portable_build_hosts_do_not_replace_newer_install_platforms(self):
        text = WORKFLOW.read_text()
        for required in (
            "{ build_os: ubuntu-22.04, target: x86_64-unknown-linux-gnu }",
            "{ build_os: ubuntu-22.04-arm, target: aarch64-unknown-linux-gnu }",
            "{ os: ubuntu-24.04, target: x86_64-unknown-linux-gnu }",
            "{ os: ubuntu-24.04-arm, target: aarch64-unknown-linux-gnu }",
            "needs.candidate.result == 'success'",
            "if-no-files-found: error",
        ):
            self.assertIn(required, text)

if __name__ == "__main__":
    unittest.main()
