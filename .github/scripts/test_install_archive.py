#!/usr/bin/env python3
"""Check the install archive's userspace and mount boundary without building or using Docker."""

from dataclasses import replace
import importlib.util
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

SCRIPT = Path(__file__).with_name("install_archive.py")
# Python -I omits the script directory from sys.path; resolve our one sibling explicitly.
CONTAINER_SPEC = importlib.util.spec_from_file_location("install_container", SCRIPT.with_name("install_container.py"))
assert CONTAINER_SPEC and CONTAINER_SPEC.loader
container = importlib.util.module_from_spec(CONTAINER_SPEC)
sys.modules[CONTAINER_SPEC.name] = container
CONTAINER_SPEC.loader.exec_module(container)
SPEC = importlib.util.spec_from_file_location("install_archive", SCRIPT)
assert SPEC and SPEC.loader
archive = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = archive
SPEC.loader.exec_module(archive)


def fixture(root="/work with spaces"):
    temporary = f"{root}/temp"
    home = f"{root}/home"
    state = f"{temporary}/blaze-fs/cache/blaze"
    return archive.Archive(
        workspace=f"{root}/source", temporary=temporary, home=home,
        cargo_home=f"{root}/custom-cargo", blaze_home=state,
        cache_home=f"{temporary}/blaze-fs/cache", store=f"{state}/v2/store",
        target=f"{state}/build/ci", llvm="/usr/local/lib/blaze-llvm/x86_64-unknown-linux-gnu",
        bash_env=f"{temporary}/blaze-sigrtmin6.sh", uv=f"{root}/uv",
        name="blaze-install-123-1", uid=1001, gid=1001, cores=2, daemon_memory_gb=6,
    )


def options(cmd, flag):
    return [cmd[i + 1] for i, arg in enumerate(cmd[:-1]) if arg == flag]


class ArchiveTests(unittest.TestCase):
    def test_old_userspace_keeps_native_release_recipe_and_abi_gate(self):
        self.assertTrue(archive.DOCKERFILE.startswith("FROM ubuntu:22.04\n"))
        self.assertIn("build-essential", archive.DOCKERFILE)
        cmd = archive.command(fixture())
        self.assertEqual(cmd[-6:], ["bash", "-e", "-o", "pipefail", "-c", archive.RECIPE])
        self.assertIn("python .github/scripts/build_release.py", archive.RECIPE)
        self.assertIn('"target":"x86_64-unknown-linux-gnu"', archive.RECIPE)
        self.assertIn('"pgo":"off"', archive.RECIPE)
        self.assertNotIn("--privileged", cmd)
        env = dict(value.split("=", 1) for value in options(cmd, "--env"))
        self.assertNotIn("BLAZE_SANDBOX", env)

    def test_custom_cargo_home_and_spaced_paths_cross_as_single_arguments(self):
        config = fixture()
        cmd = archive.command(config)
        mounts = options(cmd, "--mount")
        self.assertIn(f"type=bind,src={config.cargo_home},dst={config.cargo_home}", mounts)
        self.assertIn(f"type=bind,src={config.cargo_home}/bin,dst={config.cargo_home}/bin,readonly", mounts)
        self.assertIn(f"type=bind,src={config.temporary},dst={config.temporary}", mounts)
        self.assertIn(f"type=bind,src={config.cache_home},dst={config.home}/.cache", mounts)
        self.assertEqual(len(mounts), 7)
        self.assertFalse(any(f"src={config.home}," in mount or "dst=/etc/passwd" in mount for mount in mounts))
        self.assertFalse(any("src=/usr/lib," in mount or "src=/lib," in mount for mount in mounts))
        env = dict(value.split("=", 1) for value in options(cmd, "--env"))
        self.assertEqual(env["CARGO_HOME"], config.cargo_home)
        self.assertEqual(env["BASH_ENV"], config.bash_env)
        self.assertTrue(env["PATH"].startswith(config.cargo_home + "/bin:"))
        self.assertEqual(env["UV_PROJECT_ENVIRONMENT"], config.temporary + "/archive-venv")
        self.assertEqual(env["UV_PYTHON_INSTALL_DIR"], config.temporary + "/archive-python")
        self.assertEqual(options(cmd, "--user"), ["1001:1001"])
        self.assertEqual(options(cmd, "--ulimit"), ["memlock=-1:-1"])

    def test_explicit_resource_settings_survive_the_container_boundary(self):
        config = replace(fixture(), cores=3, daemon_memory_gb=7)
        env = dict(value.split("=", 1) for value in options(archive.command(config), "--env"))
        self.assertEqual(env["BLAZE_CORES"], "3")
        self.assertEqual(env["BLAZE_DAEMON_MEMORY_GB"], "7")
        for name in ("cores", "daemon_memory_gb"):
            for value in (0, -1, True, "6", None):
                with self.subTest(name=name, value=value), patch.object(archive.subprocess, "run") as run:
                    with self.assertRaisesRegex(ValueError, "explicit positive integer"):
                        archive.build(replace(fixture(), **{name: value}))
                    run.assert_not_called()

    def test_image_registers_its_runtime_user_with_a_writable_home_cache(self):
        config = fixture()
        commands = [json.loads(line[4:]) for line in archive.dockerfile(config).splitlines()
                    if line.startswith("RUN [")]
        self.assertEqual(commands[0], ["groupadd", "--gid", str(config.gid), "blaze"])
        self.assertEqual(options(commands[1], "--uid"), [str(config.uid)])
        self.assertEqual(options(commands[1], "--gid"), [str(config.gid)])
        self.assertEqual(options(commands[1], "--home-dir"), [config.home])
        self.assertIn("--create-home", commands[1])
        self.assertTrue(archive.RECIPE.startswith(container.PREFLIGHT))
        self.assertIn('"$HOME/.cache"', archive.RECIPE[len(container.PREFLIGHT):])

    def test_unmounted_state_and_ambiguous_mount_paths_fail_before_docker(self):
        for changes in ({"target": "/elsewhere/target"}, {"llvm": "/llvm,readonly"},
                        {"target": "/work with spaces/temp/../outside"},
                        {"workspace": "relative"}, {"name": "not a container"},
                        {"home": "/home/with:field"}, {"uid": 0}, {"gid": 0}):
            with self.subTest(changes=changes), patch.object(archive.subprocess, "run") as run:
                with self.assertRaises(ValueError):
                    archive.build(replace(fixture(), **changes))
                run.assert_not_called()

    def test_failed_build_removes_only_its_own_container(self):
        with tempfile.TemporaryDirectory() as root:
            config = replace(fixture(root), llvm=f"{root}/llvm")
            for directory in (Path(config.workspace), Path(config.temporary), Path(config.cache_home),
                              Path(config.home) / ".blaze" / "toolchains",
                              Path(config.cargo_home) / "bin", Path(config.llvm) / "bin"):
                directory.mkdir(parents=True)
            Path(config.uv).write_text("standalone uv fixture")
            Path(config.bash_env).write_text("trap '' RTMIN+6\n")
            calls = []

            def run(cmd, **kwargs):
                calls.append((cmd, kwargs))
                if cmd[1] == "run":
                    raise subprocess.TimeoutExpired(cmd, kwargs["timeout"])
                return subprocess.CompletedProcess(cmd, 0)

            with patch.object(archive.subprocess, "run", side_effect=run):
                with self.assertRaises(subprocess.TimeoutExpired):
                    archive.build(config)
            self.assertEqual(calls[0][0], ["docker", "build", "--tag", archive.IMAGE, "-"])
            self.assertEqual(calls[0][1]["input"], archive.dockerfile(config))
            self.assertEqual(calls[-1][0], ["docker", "rm", "--force", config.name])

    def test_missing_pinned_toolchain_is_reported_before_docker(self):
        with tempfile.TemporaryDirectory() as root:
            config = fixture(root)
            Path(config.workspace).mkdir()
            Path(config.temporary).mkdir()
            Path(config.cache_home).mkdir(parents=True)
            with patch.object(archive.subprocess, "run") as run:
                with self.assertRaisesRegex(ValueError, r"\.blaze/toolchains"):
                    archive.build(config)
                run.assert_not_called()

    def test_workflow_uses_new_kernel_and_prepares_reflink_before_archive(self):
        workflow = SCRIPT.parents[1] / "workflows" / "install.yaml"
        text = workflow.read_text()
        job = text.split("\n  archive:\n", 1)[1].split("\n  no-rust:\n", 1)[0]
        self.assertIn("runs-on: ubuntu-24.04\n", job)
        self.assertIn('reflink: "true"', job)
        self.assertLess(job.index('reflink: "true"'), job.index("python .github/scripts/install_archive.py"))
        self.assertIn("name: archive", job)
        self.assertIn("blaze-commit-x86_64-unknown-linux-gnu.tar.gz", job)


if __name__ == "__main__":
    unittest.main()
