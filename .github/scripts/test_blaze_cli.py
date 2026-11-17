#!/usr/bin/env python3
"""Selected-Blaze invocation and generated-fixture controls; no compiler or daemon runs."""
import contextlib
import dataclasses
import io
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "harness"))
import blaze_cli as cli  # noqa: E402
from test_distributed_ci import shell_step  # noqa: E402


def reply(argv, *, code=0, stdout="", served="daemon"):
    finished = json.dumps({
        "reason": "build-finished", "success": code == 0, "profile": "dev",
        "served": served, "took_ms": 0, "build_id": None, "units": None, "links": None,
    }, separators=(",", ":"))
    return subprocess.CompletedProcess(argv, code, finished + "\n" + stdout, "")


class InputTests(unittest.TestCase):
    def test_one_strict_frozen_value_has_no_ambient_cli_overrides(self):
        raw = {"blaze": "/selected/blaze", "projects": ["ripgrep", "fd"],
               "part": "projects", "sources": "/clones", "tmp": "/scratch",
               "output": "/reports", "keep": True}
        wanted = cli.load(json.dumps(raw))
        self.assertEqual(wanted.projects, ("ripgrep", "fd"))
        with self.assertRaises(dataclasses.FrozenInstanceError):
            wanted.keep = False
        with tempfile.TemporaryDirectory() as directory:
            source = Path(directory) / "input.json"
            source.write_text(json.dumps(raw))
            self.assertEqual(cli.load(str(source)), wanted)
        with patch.object(sys, "stdin", io.StringIO(json.dumps(raw))):
            self.assertEqual(cli.load("-"), wanted)
        with patch.dict(os.environ, {"BLAZE_BIN": "/ambient/compiler", "BLAZE_PROJECTS": "/ambient/clones"}):
            default = cli.load("{}")
        self.assertEqual(default.blaze, str(ROOT / ("target/release/blaze" + cli.EXE)))
        self.assertNotEqual(default.sources, "/ambient/clones")

    def test_unknown_types_names_and_ignored_selections_are_refused(self):
        bad = [
            [], {"unknown": True}, {"small": 1}, {"keep": "yes"}, {"blaze": None},
            {"projects": "ripgrep"}, {"projects": ["missing"]},
            {"projects": ["fd", "fd"]}, {"projects": ["fd"], "small": True},
            {"part": "checks", "small": True}, {"part": "upgraded", "projects": ["fd"]},
            {"part": "cargo"}, {"blaze": ""}, {"sources": ""}, {"output": ""},
        ]
        for value in bad:
            with self.subTest(value=value), self.assertRaises((SystemExit, ValueError)):
                cli.load(json.dumps(value))


class SelectedCompilerTests(unittest.TestCase):
    def test_sides_execute_the_exact_selection_and_copied_build_not_path_or_cargo(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            original, copied = str(root / "original/blaze"), str(root / "copied/blaze")
            calls = []

            def run(argv, **options):
                calls.append((argv, options["env"]))
                return reply(argv)

            with patch.object(cli, "BLAZE", original), patch.object(cli.subprocess, "run", side_effect=run):
                env = cli.base_env(str(root / "home"))
                env.update(PATH="/wrong/path", CARGO="/wrong/cargo")
                first = cli.Side(str(root / "workspace"), env, True)
                other = cli.Side(first.ws, dict(env, **{cli.COMPILER_ENV: copied}), True)
                first.cargo("build")
                other.cargo("build")
                first.cargo("clean", "--target-dir", first.env["CARGO_TARGET_DIR"])
                with self.assertRaisesRegex(ValueError, "cannot launch Cargo"):
                    cli.Side(first.ws, env, False)
            self.assertEqual([argv[0] for argv, _ in calls], [original, copied, original])
            self.assertTrue(all(argv[1] == "--ignore-pin" for argv, _ in calls))
            self.assertEqual(len(calls), 3)
            self.assertEqual(calls[0][1][cli.COMPILER_ENV], original)
            self.assertEqual(calls[1][1][cli.COMPILER_ENV], copied)
            self.assertEqual(first.env["CARGO_TARGET_DIR"], str(root / "workspace/target"))
            explicit = cli.Side(first.ws, dict(env, CARGO_TARGET_DIR=str(root / "custom")), True)
            self.assertEqual(explicit.env["CARGO_TARGET_DIR"], str(root / "custom"))

    def test_scratch_resets_its_target_and_store_and_stops_the_selected_compiler(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            selected = str(root / "selected/blaze")
            homes, stops = [], []

            def stop(argv, **options):
                stops.append((argv, dict(options["env"])))
                return reply(argv)

            with patch.object(cli, "BLAZE", selected), patch.object(cli, "SCRATCH_PARENT", str(root)), \
                    patch.dict(os.environ, {"BLAZE_CACHE": "/ambient/store"}), \
                    patch.object(cli.subprocess, "run", side_effect=stop):
                side = cli.Scratch(str(root / "workspace"), cli.base_env(str(root / "warm")))
                for _ in range(2):
                    with side.session() as fresh:
                        homes.append(fresh["BLAZE_HOME"])
                        self.assertEqual(fresh["BLAZE_CACHE"], str(Path(homes[-1]) / "v2/store"))
                        self.assertEqual(fresh[cli.COMPILER_ENV], selected)
                        target = Path(fresh["CARGO_TARGET_DIR"])
                        self.assertFalse(target.exists())
                        target.mkdir(parents=True)
                        (target / "old-output").write_text("previous command")
                    self.assertFalse(Path(homes[-1]).exists())
            self.assertNotEqual(homes[0], homes[1])
            self.assertEqual([argv for argv, _ in stops],
                             [[selected, "--ignore-pin", "stop"]] * 2)
            self.assertEqual([env["BLAZE_HOME"] for _, env in stops], homes)

    def test_toolchain_information_comes_from_the_exact_blaze_without_rustc(self):
        information = {"name": "blaze-2026-10-27", "nightly": "nightly-2026-10-27",
                       "rustc": "recorded", "release": "recorded", "commit": "recorded",
                       "host": "aarch64-unknown-linux-gnu", "sysroot": "/owned/toolchain"}
        answer = subprocess.CompletedProcess([], 0, json.dumps(information), "")
        with patch.object(cli, "BLAZE", "/selected/blaze"), \
                patch.dict(os.environ, {cli.COMPILER_ENV: "/ambient/blaze"}), \
                patch.object(cli.subprocess, "run", return_value=answer) as run:
            chosen = cli.selected_toolchain()
        self.assertEqual(chosen.sysroot, "/owned/toolchain")
        self.assertEqual(run.call_args.args[0], ["/selected/blaze", "--ignore-pin", "toolchain", "--json"])
        self.assertEqual(run.call_count, 1)

    def test_identical_successful_output_cannot_hide_a_non_daemon_producer(self):
        for served, accepted in (("daemon", True), ("in-process", False), ("cargo", False)):
            with self.subTest(served=served):
                ran = cli.read_build(reply([], stdout="same program output\n", served=served))
                self.assertEqual(cli.producer_verdict(ran) is None, accepted)
        self.assertIsNotNone(cli.producer_verdict(subprocess.CompletedProcess([], 0, "same output\n", "")))

    @unittest.skipIf(cli.WINDOWS, "the no-Rust PATH fixture uses Unix tool names and symlinks")
    def test_own_toolchain_does_not_reintroduce_rust_beside_the_selected_blaze(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            installed = root / "installed/bin"
            installed.mkdir(parents=True)
            for name in ("blaze", "blaze-engine", "cargo", "rustc", "rustdoc", "rustup", "cc"):
                (installed / name).write_text("a tool double, never executed\n")
            sysroot = root / "toolchain"
            cli.write(str(sysroot), "lib/rustlib/multirust-channel-manifest.toml", "fixture\n")
            chosen = SimpleNamespace(sysroot=str(sysroot), nightly="nightly-fixture", host="fixture-host")
            own = root / "user-own/.blaze/toolchains/nightly-fixture-fixture-host"
            checks, commands = [], []

            def run(argv, cwd, env, timeout=7200):
                commands.append(argv)
                self.assertEqual(argv[:2], [str(installed / "blaze"), "--ignore-pin"])
                for folder in env["PATH"].split(os.pathsep):
                    for name in ("cargo", "rustc", "rustdoc", "rustup"):
                        self.assertFalse((Path(folder) / name).exists(), (folder, name))
                self.assertTrue((root / "path-without-rust/cc").exists())
                if argv[2] == "toolchain":
                    return 0.0, subprocess.CompletedProcess(argv, 0, f"fixture, at {own}\n", "")
                main = (Path(cwd) / "src/main.rs").read_text()
                number = 48 if "6 * 8" in main else 42
                return 0.0, reply(argv, stdout=f"own {number}\n")

            with patch.object(cli, "BLAZE", str(installed / "blaze")), \
                    patch.dict(os.environ, {"PATH": str(installed)}), \
                    patch.object(cli, "selected_toolchain", return_value=chosen), \
                    patch.object(cli, "timed", side_effect=run):
                cli.own_toolchain(str(root), lambda name, held, detail="": checks.append(bool(held)))
            self.assertTrue(all(checks), checks)
            self.assertEqual([argv[2] for argv in commands], ["toolchain", "run", "run", "stop"])


class FixtureTests(unittest.TestCase):
    def test_generated_nested_builds_require_selection_and_reject_failure_and_stale_output(self):
        for source in (cli.FLAG_WORKSPACE["app/tests/it.rs"], cli.NESTED_MAIN):
            with self.subTest(source=source):
                self.assertIn(cli.NESTED_BUILD, source)
                self.assertIn('var_os("BLAZE_CLI_COMPILER")', source)
                self.assertIn('Path::new(&blaze).is_absolute()', source)
                self.assertIn('["--ignore-pin", "build", "-q", "-p", "app", "--bin", "app"]', source)
                self.assertNotIn('var("CARGO")', source)
                self.assertNotIn('Command::new("cargo")', source)
                self.assertNotIn('unwrap_or("cargo"', source)
        self.assertIn('assert!(built.success()', cli.FLAG_INTEGRATION)
        self.assertIn('if !built.success()', cli.NESTED_MAIN)
        self.assertIn('std::process::exit(built.code().unwrap_or(1))', cli.NESTED_MAIN)
        self.assertIn('said.starts_with("nested: hello nested")', cli.NESTED_MAIN)

    def test_clean_rebuild_oracle_catches_failed_or_noop_clean_and_wrong_program(self):
        for plant in ("none", "failed-clean", "noop-clean", "wrong-program"):
            with self.subTest(plant=plant), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                checks, commands = [], []

                def run(argv, cwd, env, timeout=7200):
                    commands.append(argv)
                    self.assertEqual(argv[:2], [str(root / "selected/blaze"), "--ignore-pin"])
                    command = argv[2]
                    target = Path(env["CARGO_TARGET_DIR"])
                    code, stdout = 0, ""
                    if command == "run":
                        target.mkdir(parents=True, exist_ok=True)
                        (target / "program").write_text("generated output")
                        (target / "said.rs").write_text("build script output")
                        main = (Path(cwd) / "src/main.rs").read_text()
                        text = re.search(r'println!\("([^"]+)"', main)
                        self.assertIsNotNone(text)
                        stdout = text.group(1).replace("{SAID}", "from the script") + "\n"
                        if plant == "wrong-program":
                            stdout = "stale output\n"
                    elif command == "clean":
                        if plant == "failed-clean":
                            code = 23
                        elif plant != "noop-clean":
                            shutil.rmtree(target)
                    return 0.0, reply(argv, code=code, stdout=stdout)

                with patch.object(cli, "BLAZE", str(root / "selected/blaze")), \
                        patch.object(cli, "timed", side_effect=run), \
                        patch.object(cli, "daemon_pid", return_value=41), \
                        contextlib.redirect_stdout(io.StringIO()):
                    cli.cleaned(str(root), lambda name, held, detail="": checks.append((name, bool(held))))
                self.assertEqual(all(held for _, held in checks), plant == "none")
                cleans = [argv for argv in commands if argv[2] == "clean"]
                self.assertEqual(len(cleans), 2)
                self.assertNotIn("--target-dir", cleans[0])
                self.assertIn("--target-dir", cleans[1])
                self.assertEqual(len([name for name, _ in checks if "actual target directory" in name]), 3)


class CallerTests(unittest.TestCase):
    def test_train_callers_pass_one_typed_input_and_preserve_the_harness_exit(self):
        workflow = ROOT / ".github/workflows/blaze-platform.yaml"
        for lane, step in (("cli", "selected Blaze CLI checks and projects"),
                           ("projects", "selected Blaze larger projects")):
            for group, code in (("checks", 0), ("ripgrep hyperfine", 0), ("checks", 23)):
                with self.subTest(lane=lane, group=group, code=code), tempfile.TemporaryDirectory() as directory:
                    root = Path(directory)
                    evidence, commands = root / f"{lane}-evidence", root / "commands"
                    evidence.mkdir()
                    commands.mkdir()
                    uv = commands / "uv"
                    uv.write_text(
                        "#!/bin/sh\n"
                        'printf \'%s\\n\' "$@" > "$RUNNER_TEMP/uv-argv.txt"\n'
                        'for last; do :; done\n'
                        'cp "$last" "$RUNNER_TEMP/caller.json"\n'
                        f"printf '%s\\n' 'harness fixture exit {code}'\nexit {code}\n")
                    uv.chmod(0o700)
                    env = {**os.environ, "RUNNER_TEMP": str(root), "CLI_GROUP": group,
                           "TMPDIR": str(root), "PATH": str(commands) + os.pathsep + os.environ["PATH"]}
                    done = subprocess.run(
                        ["bash", "-e", "-o", "pipefail", "-c", shell_step(workflow, step)],
                        env=env, capture_output=True, text=True, timeout=10)
                    self.assertEqual(done.returncode, code, done.stdout + done.stderr)
                    given = cli.load(str(root / "caller.json"))
                    self.assertEqual(given.blaze, str(root / f"{lane}-binaries/blaze"))
                    self.assertEqual(given.part, "checks" if group == "checks" else "projects")
                    self.assertEqual(given.projects, () if group == "checks" else ("ripgrep", "hyperfine"))
                    self.assertEqual(given.tmp, str(root))
                    self.assertEqual(given.output, str(evidence))
                    argv = (root / "uv-argv.txt").read_text().splitlines()
                    self.assertEqual(argv[-2:], ["blaze_cli.py", str(evidence / "input.json")])


if __name__ == "__main__":
    unittest.main()
