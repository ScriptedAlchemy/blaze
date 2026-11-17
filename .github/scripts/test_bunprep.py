#!/usr/bin/env python3
"""Check native preparation's command boundary without building Bun or running a compiler."""

import importlib.util
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest import mock


SOURCE = Path(__file__).resolve().parents[2] / "harness" / "bunprep.py"
SPEC = importlib.util.spec_from_file_location("bunprep", SOURCE)
assert SPEC and SPEC.loader
prep = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = prep
SPEC.loader.exec_module(prep)


class NativePreparationTests(unittest.TestCase):
    def prepare(self, *, plant=False):
        with tempfile.TemporaryDirectory() as directory:
            commands = []

            def invoked(argv, **_kwargs):
                commands.append(list(argv))
                return subprocess.CompletedProcess(argv, 0)

            def overlay(_tree):
                if plant:
                    subprocess.run(["cargo", "metadata", "--format-version", "1"], check=True)

            with mock.patch.dict(os.environ, {"BUN_BOOTSTRAP": "/selected/bun", "SDKROOT": "/sdk"}), \
                    mock.patch.object(prep, "system_parts", return_value=None), \
                    mock.patch.object(prep, "edit"), \
                    mock.patch.object(prep, "native_targets", return_value=["native.o"]), \
                    mock.patch.object(prep, "link_args"), \
                    mock.patch.object(prep, "overlay", side_effect=overlay), \
                    mock.patch.object(prep.subprocess, "run", side_effect=invoked):
                prep.native(directory, "a" * 40)
                self.assertEqual((Path(directory) / prep.MARKER).read_text(),
                                 f"{'a' * 40} {prep.SELF}\n")
            return commands

    def assert_native_only(self, commands):
        self.assertEqual([Path(command[0]).name for command in commands], ["bun", "ninja"])
        self.assertIn("--configure-only", commands[0])
        self.assertEqual(commands[1][-1], "native.o")

    def test_preparation_leaves_rust_resolution_to_the_selected_blaze_build(self):
        self.assert_native_only(self.prepare())

    def test_the_oracle_catches_an_ambient_cargo_metadata_step(self):
        with self.assertRaises(AssertionError):
            self.assert_native_only(self.prepare(plant=True))

    def test_preparation_does_not_select_an_ambient_rust_toolchain(self):
        given = {"PATH": "/native/bin", "SDKROOT": "/sdk", "RUSTUP_TOOLCHAIN": "other",
                 "RUSTC": "/ambient/rustc", "RUSTFLAGS": "-Copt-level=0",
                 "RUSTC_WRAPPER": "wrapper", "RUSTC_WORKSPACE_WRAPPER": "wrapper",
                 "CARGO_BUILD_TARGET": "other", "CARGO_TARGET_DIR": "/other"}
        self.assertEqual(prep.rust_env(given), {"PATH": "/native/bin", "SDKROOT": "/sdk"})
        self.assertEqual(set(prep.env_of("/prepared")), {"BUN_CODEGEN_DIR", "BUN_NATIVE_LINK_ARGS"})

    def test_typed_input_rejects_missing_unknown_and_invalid_checkout(self):
        self.assertEqual(prep.load('{"checkout":"/source/bun"}').checkout, "/source/bun")
        for value in ({}, {"checkout": ""}, {"checkout": 7}, {"checkout": "bad\0path"},
                      {"checkout": "/source/bun", "unknown": True}, []):
            with self.subTest(value=value), self.assertRaises((TypeError, ValueError)):
                prep.load(json.dumps(value))


if __name__ == "__main__":
    unittest.main()
