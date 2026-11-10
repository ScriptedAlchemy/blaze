#!/usr/bin/env python3
"""Check the install archive's userspace and mount boundary without building or using Docker."""

from dataclasses import replace
import importlib.util
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

SCRIPT = Path(__file__).with_name("install_archive.py")
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
        name="blaze-install-123-1", uid=1001, gid=1001,
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
        self.assertEqual(len(mounts), 6)
        self.assertFalse(any("src=/usr/lib," in mount or "src=/lib," in mount for mount in mounts))
        env = dict(value.split("=", 1) for value in options(cmd, "--env"))
        self.assertEqual(env["CARGO_HOME"], config.cargo_home)
        self.assertEqual(env["BASH_ENV"], config.bash_env)
        self.assertTrue(env["PATH"].startswith(config.cargo_home + "/bin:"))
        self.assertEqual(env["UV_PROJECT_ENVIRONMENT"], config.temporary + "/archive-venv")
        self.assertEqual(env["UV_PYTHON_INSTALL_DIR"], config.temporary + "/archive-python")
        self.assertEqual(options(cmd, "--user"), ["1001:1001"])
        self.assertEqual(options(cmd, "--ulimit"), ["memlock=-1:-1"])

    def test_unmounted_state_and_ambiguous_mount_paths_fail_before_docker(self):
        for changes in ({"target": "/elsewhere/target"}, {"llvm": "/llvm,readonly"},
                        {"workspace": "relative"}, {"name": "not a container"}):
            with self.subTest(changes=changes), patch.object(archive.subprocess, "run") as run:
                with self.assertRaises(ValueError):
                    archive.build(replace(fixture(), **changes))
                run.assert_not_called()

    def test_failed_build_removes_only_its_own_container(self):
        with tempfile.TemporaryDirectory() as root:
            config = replace(fixture(root), llvm=f"{root}/llvm")
            for directory in (Path(config.workspace), Path(config.temporary),
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
            self.assertEqual(calls[0][1]["input"], archive.DOCKERFILE)
            self.assertEqual(calls[-1][0], ["docker", "rm", "--force", config.name])

    def test_missing_pinned_toolchain_is_reported_before_docker(self):
        with tempfile.TemporaryDirectory() as root:
            config = fixture(root)
            Path(config.workspace).mkdir()
            Path(config.temporary).mkdir()
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
