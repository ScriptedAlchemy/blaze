#!/usr/bin/env python3
"""Exercise the installer smoke oracle with process doubles, never a compiler or Docker."""

from dataclasses import replace
import importlib.util
import json
import os
from pathlib import Path
import shlex
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

SCRIPT = Path(__file__).with_name("install_without_rust.py")
# Python -I omits the script directory from sys.path; resolve our one sibling explicitly.
CONTAINER_SPEC = importlib.util.spec_from_file_location("install_container", SCRIPT.with_name("install_container.py"))
assert CONTAINER_SPEC and CONTAINER_SPEC.loader
container = importlib.util.module_from_spec(CONTAINER_SPEC)
sys.modules[CONTAINER_SPEC.name] = container
CONTAINER_SPEC.loader.exec_module(container)
SPEC = importlib.util.spec_from_file_location("install_without_rust", SCRIPT)
assert SPEC and SPEC.loader
smoke = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = smoke
SPEC.loader.exec_module(smoke)


DOUBLE = r'''
import json, os, pathlib, re, shutil, sys
root = pathlib.Path(os.environ["TEST_ROOT"])
config = json.loads((root / "config.json").read_text())
name = pathlib.Path(sys.argv[0]).stem
with (root / "calls.jsonl").open("a") as file:
    file.write(json.dumps([name, sys.argv[1:], pathlib.Path.cwd().name]) + "\n")
if name == "sh":
    assert not sys.argv[1:]
    home = pathlib.Path(os.environ["HOME"])
    (home / ".cargo/bin").mkdir(parents=True)
    (home / ".blaze/toolchains").mkdir(parents=True)
    shutil.copy2(root / "bin/blaze", home / ".cargo/bin/blaze")
    if config.get("rust_after_install"):
        shutil.copy2(root / "bin/blaze", home / ".cargo/bin/cargo")
elif name == "blaze":
    if sys.argv[1:] == ["stop"]:
        sys.exit(0)
    assert sys.argv[1:] == ["run"], sys.argv
    if pathlib.Path.cwd().name == "hello":
        print("wrong hello" if config.get("bad_hello") else "Hello, world!")
        if config.get("foreign_build"):
            print("the daemon has no part", file=sys.stderr)
    else:
        value = re.search(r"Point \{ x: (\d+) \}", pathlib.Path("src/main.rs").read_text()).group(1)
        print("derived " + ("1" if config.get("stale_derive") else value))
        if config.get("foreign_macro"):
            print("did not make the proc macro", file=sys.stderr)
else:
    sys.exit(99)
'''


def fixture():
    return smoke.Smoke(
        workspace="/job/source", temporary="/job/temp", image="debian:trixie-slim",
        packages="apt", system="Debian 13", blaze_home="/job/temp/blaze-fs/cache/blaze",
        cache_home="/job/temp/blaze-fs/cache", store="/job/temp/blaze-fs/cache/blaze/v2/store",
        target="/job/temp/blaze-fs/cache/blaze/build/ci", summary="/job/temp/summary",
        name="blaze-install-smoke-123-1", uid=1001, gid=1001,
    )


def options(cmd, flag):
    return [cmd[i + 1] for i, arg in enumerate(cmd[:-1]) if arg == flag]


class InstallerTests(unittest.TestCase):
    def exercise(self, **config):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            binary = root / "bin"
            binary.mkdir()
            for name in ("sh", "blaze"):
                implementation = root / (name + ".py")
                implementation.write_text(DOUBLE)
                command = binary / name
                command.write_text("#!/bin/sh\nexec " + shlex.quote(sys.executable) + " -I -S "
                                   + shlex.quote(str(implementation)) + ' "$@"\n')
                command.chmod(0o700)
            (root / "site").mkdir()
            (root / "site/install.sh").write_text("installer double input")
            (root / "config.json").write_text(json.dumps(config))
            env = {key: value for key, value in os.environ.items()
                   if not key.startswith(("BLAZE_", "CARGO_", "RUST")) and key != "BASH_ENV"}
            env.update(TEST_ROOT=str(root), HOME=str(root / "home"), RUNNER_TEMP=str(root),
                       PATH=str(binary) + ":/usr/bin:/bin", BLAZE_INSTALL_SYSTEM="fixture distro",
                       GITHUB_STEP_SUMMARY=str(root / "summary"))
            result = subprocess.run(["/bin/sh", "-eu", "-c", smoke.INSTALL + smoke.DERIVE],
                                    cwd=root, env=env, text=True, capture_output=True, timeout=30)
            calls_file = root / "calls.jsonl"
            calls = [json.loads(line) for line in calls_file.read_text().splitlines()] if calls_file.exists() else []
            return result, calls

    def test_installs_this_archive_and_runs_hello_then_both_derive_revisions(self):
        result, calls = self.exercise()
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual([cwd for name, args, cwd in calls if name == "blaze" and args == ["run"]],
                         ["hello", "derive", "derive"])
        self.assertEqual(sum(name == "sh" for name, _, _ in calls), 1)
        self.assertIn('BLAZE_ARCHIVE="$PWD/blaze-commit-x86_64-unknown-linux-gnu.tar.gz"', smoke.INSTALL)

    def test_oracle_rejects_wrong_output_foreign_compilation_and_stale_macro_edit(self):
        for control in ("bad_hello", "foreign_build", "stale_derive", "foreign_macro", "rust_after_install"):
            with self.subTest(control=control):
                result, _ = self.exercise(**{control: True})
                self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)

    def test_container_has_fresh_state_and_no_host_compiler_inputs(self):
        config = fixture()
        cmd = smoke.command(config)
        self.assertEqual(options(cmd, "--mount"), [
            "type=bind,src=/job/source,dst=/job/source,readonly",
            "type=bind,src=/job/temp,dst=/job/temp",
        ])
        env = dict(value.split("=", 1) for value in options(cmd, "--env"))
        self.assertEqual(env["HOME"], "/job/temp/blaze-fs/cache/install-home")
        self.assertEqual(env["CARGO_HOME"], env["HOME"] + "/.cargo")
        self.assertEqual(env["RUNNER_TEMP"], env["HOME"] + "/work")
        self.assertNotIn(".cargo", env["PATH"])
        self.assertNotIn("LLVM_SYS_231_PREFIX", env)
        self.assertNotIn("BLAZE_SANDBOX", env)
        self.assertNotIn("--privileged", cmd)
        self.assertEqual(options(cmd, "--ulimit"), ["memlock=-1:-1"])
        self.assertEqual(options(cmd, "--user"), ["1001:1001"])

    def test_all_distributions_register_the_numeric_user_at_its_fresh_home(self):
        for image, packages in (("debian:trixie-slim", "apt"), ("debian:bookworm-slim", "apt"),
                                ("ubuntu:22.04", "apt"), ("registry.access.redhat.com/ubi9/ubi", "dnf")):
            with self.subTest(image=image):
                config = replace(fixture(), image=image, packages=packages, uid=2002, gid=2003)
                commands = [json.loads(line[4:]) for line in smoke.dockerfile(config).splitlines()
                            if line.startswith("RUN [")]
                self.assertEqual(commands[0], ["groupadd", "--gid", "2003", "blaze"])
                self.assertEqual(options(commands[1], "--uid"), ["2002"])
                self.assertEqual(options(commands[1], "--gid"), ["2003"])
                env = dict(value.split("=", 1) for value in options(smoke.command(config), "--env"))
                self.assertEqual(options(commands[1], "--home-dir"), [env["HOME"]])
                self.assertIn("--no-create-home", commands[1])
                self.assertIn("shadow-utils" if packages == "dnf" else "passwd", smoke.PACKAGES[packages])
        self.assertTrue(smoke.PREFLIGHT.startswith(container.PREFLIGHT))
        self.assertIn('"$HOME/.cache"', smoke.PREFLIGHT[len(container.PREFLIGHT):])

    def test_home_cache_is_prepared_before_the_container_starts(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            temporary = root / "temp"
            state = temporary / "blaze-fs/cache/blaze"
            config = replace(fixture(), workspace=str(root / "source"), temporary=str(temporary),
                             blaze_home=str(state), cache_home=str(state.parent),
                             store=str(state / "v2/store"), target=str(state / "build/ci"),
                             summary=str(temporary / "summary"))
            for path in (Path(config.workspace), Path(config.store), Path(config.target), state / "std"):
                path.mkdir(parents=True)
            (Path(config.workspace) / "blaze-commit-x86_64-unknown-linux-gnu.tar.gz").write_text("archive fixture")
            calls = []

            def run(cmd, **kwargs):
                calls.append((cmd, kwargs))
                self.assertTrue((smoke.fresh_home(config) / ".cache").is_dir())
                return subprocess.CompletedProcess(cmd, 0)

            with patch.object(smoke.subprocess, "run", side_effect=run):
                smoke.run(config)
            self.assertEqual(calls[0][1]["input"], smoke.dockerfile(config))
            self.assertEqual(options(calls[1][0], "--user"), ["1001:1001"])
            self.assertEqual((smoke.fresh_home(config) / ".cache").stat().st_mode & 0o777, 0o700)

    def test_missing_state_bind_and_malformed_image_are_refused(self):
        for changes in ({"target": "/unmounted/target"},
                        {"target": "/job/temp/../unmounted/target"},
                        {"summary": "/job/temp/../outside-summary"},
                        {"image": "ubuntu:22.04\nRUN echo wrong"},
                        {"blaze_home": "/job/temp/blaze:fs/cache/blaze"}, {"uid": 0}, {"gid": 0}):
            with self.subTest(changes=changes), self.assertRaises(ValueError):
                smoke.command(replace(fixture(), **changes))


if __name__ == "__main__":
    unittest.main()
