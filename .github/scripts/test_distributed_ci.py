#!/usr/bin/env python3
"""CI refuses wrong build identities and incomplete distributed acceptance.

The small executables below are identity fixtures and shell command doubles. No Blaze
build or daemon runs here; real distributed acceptance belongs to the dependent job.
"""
import contextlib
import copy
import importlib.util
import io
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

import dist_build_identity as identity

ROOT = Path(__file__).resolve().parents[2]
MAIN = ROOT / ".github/workflows/blaze.yaml"
PLATFORM = ROOT / ".github/workflows/blaze-platform.yaml"
PASSED = "distv2: every check passed\n"


def shell_step(workflow: Path, name: str) -> str:
    """Exercise the shell CI runs, so removing a safeguard changes the test's result."""
    lines = workflow.read_text().splitlines()
    heading = f"      - name: {name}"
    if lines.count(heading) != 1:
        raise AssertionError(f"{workflow.name} must have exactly one {name} step")
    at = lines.index(heading) + 1
    while at < len(lines) and lines[at] != "        run: |":
        if lines[at].startswith("      - "):
            raise AssertionError(f"{name} has no shell body")
        at += 1
    if at == len(lines):
        raise AssertionError(f"{name} has no shell body")
    body = []
    for line in lines[at + 1:]:
        if line and not line.startswith("          "):
            break
        body.append(line[10:] if line else "")
    return "\n".join(body) + "\n"


class BuildIdentityTests(unittest.TestCase):
    def setUp(self):
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        self.root = Path(directory.name)
        self.repo = self.root / "repo"
        self.repo.mkdir()
        self.bin = self.root / "bin"
        self.bin.mkdir()
        self.git("init", "-q")
        (self.repo / "source.rs").write_text("source used to build the binaries\n")
        (self.repo / ".gitignore").write_text((ROOT / ".gitignore").read_text())
        self.git("add", ".")
        self.git("commit", "-qm", "source fixture")
        suffix = ".exe" if sys.platform == "win32" else ""
        self.programs = {}
        for stem in ("blaze", "blaze-engine", "blaze-dist-local", "blaze-serve"):
            program = self.bin / (stem + suffix)
            raw = f"executable identity fixture for {stem}\n".encode()
            program.write_bytes(raw)
            program.chmod(0o700)
            self.programs[program] = raw
        changed_root = patch.object(identity, "ROOT", self.repo)
        changed_root.start()
        self.addCleanup(changed_root.stop)

    def git(self, *args):
        return subprocess.check_output(
            ["git", "-C", str(self.repo), "-c", "user.name=fixture",
             "-c", "user.email=fixture@example.invalid", *args], stderr=subprocess.PIPE, timeout=10)

    def command(self, command="verify", *, services=False):
        with contextlib.redirect_stdout(io.StringIO()):
            identity.main(identity.Input(command, str(self.bin), services))

    def test_handoff_rejects_changed_or_missing_members_of_each_stage(self):
        for services in (False, True):
            with self.subTest(services=services):
                self.command("seal", services=services)
                self.command(services=services)
                record = json.loads((self.bin / identity.MANIFEST).read_text())
                self.assertEqual(record["source"]["tree"], self.git("rev-parse", "HEAD^{tree}").decode().strip())
                for name in record["binaries"]:
                    program = self.bin / name
                    program.write_bytes(b"a different executable")
                    with self.assertRaises(identity.Refused):
                        self.command(services=services)
                    program.unlink()
                    with self.assertRaises(identity.Refused):
                        self.command(services=services)
                    program.write_bytes(self.programs[program])
                    program.chmod(0o700)
                self.command(services=services)

    def test_same_tree_at_another_commit_is_not_the_builds_source(self):
        self.command("seal")
        self.git("commit", "--allow-empty", "-qm", "another named commit")
        with self.assertRaisesRegex(identity.Refused, "build source"):
            self.command()

    def test_source_patch_or_untracked_source_cannot_be_omitted_from_identity(self):
        self.command("seal")
        tracked = self.repo / "source.rs"
        original = tracked.read_text()
        tracked.write_text("changed compilation input\n")
        with self.assertRaisesRegex(identity.Refused, "checkout has changes"):
            self.command()
        tracked.write_text(original)
        (self.repo / "injected.rs").write_text("untracked compilation input\n")
        with self.assertRaisesRegex(identity.Refused, "checkout has changes"):
            self.command()

    def test_verified_reflink_target_symlink_is_build_output(self):
        (self.repo / "target").symlink_to(self.bin, target_is_directory=True)
        self.command("seal")
        self.command()
        # Another untracked symlink is still refused; the rule is the exact CI output name.
        (self.repo / "not-target").symlink_to(self.bin, target_is_directory=True)
        with self.assertRaises(identity.Refused):
            self.command()

    def test_schema_and_executable_set_are_exact(self):
        actual = identity.identity(self.bin, False)
        for change in (lambda value: value.update(schema=True),
                       lambda value: value.update(unknown="not in schema"),
                       lambda value: value["binaries"].pop(next(iter(value["binaries"]))),
                       lambda value: value["binaries"].update(other="not the built programs")):
            altered = copy.deepcopy(actual)
            change(altered)
            with self.assertRaises(identity.Refused):
                identity.verify(altered, actual)


class AcceptanceGateTests(unittest.TestCase):
    def run_shell(self, workflow, step, env):
        return subprocess.run(
            ["bash", "-e", "-o", "pipefail", "-c", shell_step(workflow, step)],
            env={**os.environ, **env}, text=True, capture_output=True, timeout=10)

    def test_aggregate_requires_real_acceptance_for_every_code_tier(self):
        base = {"changes": {"result": "success", "outputs": {"tier": "gate"}},
                "scripts": {"result": "success", "outputs": {}},
                "linux-arm64": {"result": "success", "outputs": {
                    "distributed_acceptance": "true", "forkfixes_acceptance": "true"}}}
        cases = [("gate passes", base, True)]
        warm = copy.deepcopy(base)
        warm["changes"]["outputs"]["tier"] = "warm"
        cases.append(("warm passes", warm, True))
        docs = copy.deepcopy(base)
        docs["changes"]["outputs"]["tier"] = "none"
        docs["linux-arm64"] = {"result": "skipped", "outputs": {}}
        cases.append(("docs need no build", docs, True))
        for missing in ({}, {"distributed_acceptance": ""}, {"distributed_acceptance": "false"},
                        {"distributed_acceptance": "true"}, {"forkfixes_acceptance": "true"},
                        {"distributed_acceptance": "true", "forkfixes_acceptance": "false"}):
            changed = copy.deepcopy(base)
            changed["linux-arm64"]["outputs"] = missing
            cases.append((f"missing acceptance {missing}", changed, False))
        for job, result in (("linux-arm64", "skipped"), ("linux-arm64", "cancelled"),
                            ("scripts", "failure"), ("scripts", "cancelled"), ("changes", "skipped")):
            changed = copy.deepcopy(base)
            changed[job]["result"] = result
            cases.append((f"{job} {result}", changed, False))
        for name, needs, wanted in cases:
            with self.subTest(name=name):
                done = self.run_shell(MAIN, "every required check passed", {"NEEDS": json.dumps(needs)})
                self.assertEqual(done.returncode == 0, wanted, done.stdout + done.stderr)

    def shell_fixture(self, root, *, code=0):
        evidence = root / "dist-evidence"
        evidence.mkdir()
        commands = root / "commands"
        commands.mkdir()
        python = commands / "python3"
        python.write_text(
            "#!/bin/sh\n"
            'if [ "$1" = .github/scripts/dist_build_identity.py ]; then cat >/dev/null; fi\n'
            f"printf '%s\\n' 'distv2: every check passed'\nexit {code}\n")
        python.chmod(0o700)
        return evidence, {"RUNNER_TEMP": str(root), "GITHUB_OUTPUT": str(root / "output"),
                          "PATH": str(commands) + os.pathsep + os.environ["PATH"]}

    def test_missing_or_non_fault_run_cannot_issue_an_acceptance_receipt(self):
        for dev, release, wanted in ((PASSED, PASSED, True), (None, PASSED, False),
                                     (PASSED, None, False),
                                     (PASSED, "distv2: non-fault checks passed; fault acceptance NOT RUN\n", False)):
            with self.subTest(dev=dev, release=release), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                evidence, env = self.shell_fixture(root)
                for profile, text in (("dev", dev), ("release", release)):
                    if text is not None:
                        (evidence / f"{profile}.log").write_text(text)
                done = self.run_shell(PLATFORM, "both complete fault runs used unchanged binaries and source", env)
                self.assertEqual(done.returncode == 0, wanted, done.stdout + done.stderr)
                output = root / "output"
                self.assertEqual(output.read_text() if output.exists() else "", "accepted=true\n" if wanted else "")

    def test_tee_preserves_a_harness_failure_even_with_a_success_line(self):
        for step in ("dev two-worker, cold-cache, cross-PR and fault acceptance",
                     "release and folded-group two-worker and fault acceptance"):
            with self.subTest(step=step), tempfile.TemporaryDirectory() as directory:
                _, env = self.shell_fixture(Path(directory), code=23)
                done = self.run_shell(PLATFORM, step, env)
                self.assertEqual(done.returncode, 23, done.stdout + done.stderr)

    def test_source_built_protocol_test_failure_reaches_ci(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            _, env = self.shell_fixture(root)
            binaries = root / "dist-binaries"
            binaries.mkdir()
            blaze = binaries / "blaze"
            blaze.write_text("#!/bin/sh\nprintf '%s\\n' 'a protocol test failed'\nexit 23\n")
            blaze.chmod(0o700)
            done = self.run_shell(PLATFORM, "distributed protocol and carrier tests with Stage 1", env)
            self.assertEqual(done.returncode, 23, done.stdout + done.stderr)

    def test_fork_migration_harness_failure_reaches_ci(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            _, env = self.shell_fixture(root, code=23)
            (root / "forkfixes-evidence").mkdir()
            done = self.run_shell(PLATFORM, "every fork migration fixture with Stage 1", env)
            self.assertEqual(done.returncode, 23, done.stdout + done.stderr)

    def test_ci_generated_inputs_keep_explicit_lock_policy_in_child_environment(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            evidence, env = self.shell_fixture(root)
            binaries = root / "dist-binaries"
            binaries.mkdir()
            (binaries / "blaze-build.json").write_text("{}\n")
            services = root / "blaze-fs/dist-service-build/release"
            services.mkdir(parents=True)
            for name in ("blaze-dist-local", "blaze-serve"):
                (services / name).write_text("identity fixture; never executed\n")
            # Run the real input-generation shell, with only its identity command doubled.
            done = self.run_shell(PLATFORM, "bind the four acceptance programs to the same source", env)
            self.assertEqual(done.returncode, 0, done.stdout + done.stderr)

            spec = importlib.util.spec_from_file_location("distv2_ci_input", ROOT / "harness/distv2.py")
            self.assertIsNotNone(spec)
            self.assertIsNotNone(spec.loader)
            harness = importlib.util.module_from_spec(spec)
            with patch.object(sys, "path", [str(ROOT / "harness"), *sys.path]), patch.dict(sys.modules), \
                    patch.dict(os.environ, {"CI": "true", "BLAZE_LOCK": "locked"}):
                sys.modules[spec.name] = harness
                spec.loader.exec_module(harness)
                for profile in ("dev", "release"):
                    with self.subTest(profile=profile):
                        generated = evidence / f"{profile}-input.json"
                        given = harness.load(str(generated))
                        # Environment construction only: no constructor socket, daemon or build.
                        run = object.__new__(harness.Run)
                        run.given = given
                        run.root = Path(given.scratch)
                        run.root.mkdir()
                        run.socket, run.port = root / "unused.sock", 1
                        for distributed in (False, True):
                            child = run.env("solo" if not distributed else "a", distributed)
                            self.assertEqual(child["CI"], "true")
                            self.assertEqual(child.get("BLAZE_LOCK"), "write")
                        # Removing the explicit input loses the lock setting despite CI and
                        # an inherited BLAZE_LOCK. This is why setting only the job env fails.
                        without_policy = json.loads(generated.read_text())
                        without_policy["env"].pop("BLAZE_LOCK")
                        run.given = harness.load(json.dumps(without_policy))
                        self.assertNotIn("BLAZE_LOCK", run.env("solo", False))


if __name__ == "__main__":
    unittest.main()
