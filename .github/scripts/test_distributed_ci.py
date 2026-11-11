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
import tarfile
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
                "workflows": {"result": "success"},
                "linux-6-8": {"result": "success"},
                "workspace-check": {"result": "success"},
                "distributed-core": {"result": "success"},
                "macos": {"result": "skipped"},
                "linux-arm64": {"result": "success", "outputs": {
                    "distributed_acceptance": "true", "forkfixes_acceptance": "true",
                    "protocol_tests": "true"}}}
        cases = [("gate passes", base, True)]
        warm = copy.deepcopy(base)
        warm["changes"]["outputs"]["tier"] = "warm"
        warm["macos"]["result"] = "success"
        cases.append(("warm passes", warm, True))
        missing_warm_macos = copy.deepcopy(warm)
        missing_warm_macos["macos"]["result"] = "skipped"
        cases.append(("warm with skipped macOS", missing_warm_macos, False))
        docs = copy.deepcopy(base)
        docs["changes"]["outputs"]["tier"] = "none"
        docs["linux-arm64"] = {"result": "skipped", "outputs": {}}
        docs["linux-6-8"]["result"] = "skipped"
        docs["workspace-check"]["result"] = "skipped"
        docs["distributed-core"]["result"] = "skipped"
        cases.append(("docs need no build", docs, True))
        for receipt in ("distributed_acceptance", "forkfixes_acceptance", "protocol_tests"):
            for value in (None, "", "false"):
                changed = copy.deepcopy(base)
                outputs = changed["linux-arm64"]["outputs"]
                if value is None:
                    del outputs[receipt]
                else:
                    outputs[receipt] = value
                cases.append((f"missing acceptance {receipt}={value!r}", changed, False))
        for job, result in (("linux-arm64", "skipped"), ("linux-arm64", "cancelled"),
                            ("scripts", "failure"), ("scripts", "cancelled"), ("scripts", "skipped"),
                            ("workflows", "skipped"), ("linux-6-8", "skipped"),
                            ("workspace-check", "skipped"), ("distributed-core", "skipped"),
                            ("distributed-core", "failure"), ("changes", "skipped")):
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
            'if [ "$1" = .github/scripts/dist_build_identity.py ]; then '
            'cat > "$RUNNER_TEMP/identity-input.json"; fi\n'
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

    def test_partial_b0_library_feedback_requires_every_model_and_control(self):
        models = [
            "retirement_preserves_survivors_and_exact_assignment_ownership",
            "cancellation_cleans_a_promotion_the_worker_has_not_observed",
            "stale_claimed_helpers_do_not_block_available_replacements",
            "execution_bindings_retain_cleanup_without_sliding_replay_or_expiry",
        ]
        controls = [
            "the_oracle_catches_leaving_without_retirement",
            "the_oracle_catches_release_without_a_pending_stop",
            "the_oracle_catches_a_stale_retirement_of_the_replacement_lead",
            "the_oracle_catches_a_stale_acknowledgment_of_a_reused_slot",
            "the_oracle_catches_an_unowned_retirement",
            "the_oracle_catches_retired_work_kept_under_its_old_lease",
            "the_oracle_catches_promotion_erasing_the_continuing_owner",
            "the_oracle_catches_cleanup_using_only_the_last_observed_attempt",
            "the_oracle_catches_a_stale_claimed_helper_misclassified_as_unclaimed",
            "the_oracle_catches_a_missing_verified_execution_binding",
            "the_oracle_catches_a_sliding_session_ceiling",
            "the_oracle_catches_a_lost_reply_with_changed_replay_identity",
            "the_oracle_catches_different_bytes_reusing_a_replay_identity",
        ]
        names = models + controls
        # Read the actual job's literal setting, so removing verification from the workflow
        # cannot be hidden by this shell fixture supplying its own unrelated default.
        header = MAIN.read_text().split("  distributed-core:\n", 1)[1].split("    steps:\n", 1)[0]
        mode = [line.split(":", 1)[1].strip().strip("\"'") for line in header.splitlines()
                if line.startswith("      BLAZE_TESTS:")]
        self.assertEqual(mode, ["verify"])
        cases = [("complete", names, 0, 0), ("zero tests", [], 0, 1)]
        # Each missing line is the only fault; all remaining cases still report success.
        cases += [(f"missing model {name}", [item for item in names if item != name], 0, 1)
                  for name in models]
        cases += [(f"missing control {name}", [item for item in names if item != name], 0, 1)
                  for name in controls]
        cases.append(("compiler failed after success lines", names, 23, 23))
        for label, reported, code, expected in cases:
            with self.subTest(case=label), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                commands = root / "commands"
                commands.mkdir()
                blaze = commands / "blaze"
                blaze.write_text(
                    "#!/bin/sh\n"
                    'printf \'%s\\n\' "$0" "$@" > "$RUNNER_TEMP/core-evidence/invocation.txt"\n'
                    'printf \'%s\\n\' "$BLAZE_TESTS" > "$RUNNER_TEMP/core-evidence/mode.txt"\n'
                    'cat "$CORE_OUTPUT"\nexit "$CORE_EXIT"\n')
                blaze.chmod(0o700)
                output = f"running {len(reported)} tests\n"
                output += "".join(f"test fixture::{name} ... ok\n" for name in reported)
                output += f"test result: ok. {len(reported)} passed; 0 failed; 0 ignored\n"
                transcript = root / "test-output.txt"
                transcript.write_text(output)
                env = {"RUNNER_TEMP": str(root), "GITHUB_WORKSPACE": str(ROOT),
                       "PATH": str(commands) + os.pathsep + os.environ["PATH"],
                       "BLAZE_TESTS": mode[0], "CORE_OUTPUT": str(transcript), "CORE_EXIT": str(code)}
                done = self.run_shell(MAIN, "distributed library models and planted controls with B0", env)
                self.assertEqual(done.returncode, expected, done.stdout + done.stderr)
                evidence = root / "core-evidence"
                invocation = (evidence / "invocation.txt").read_text().splitlines()
                self.assertEqual(invocation, [str(blaze), "--ignore-pin", "test", "--release", "--locked",
                                              "--lib", "-p", "blaze-dist", "-p", "blaze-dist-local", "--no-fail-fast"])
                self.assertEqual((evidence / "mode.txt").read_text(), "verify\n")
                self.assertEqual((evidence / "tests.log").read_text(), output)
                self.assertEqual((evidence / "bootstrap-pin.txt").read_bytes(),
                                 (ROOT / ".github/blaze-version").read_bytes())
                self.assertTrue((evidence / "bootstrap-sha256.txt").read_text().rstrip().endswith("  " + str(blaze)))
                if expected == 1:
                    missing = next(name for name in names if name not in reported)
                    self.assertIn("missing executed library control: " + missing, done.stdout)

    def test_tee_preserves_a_harness_failure_even_with_a_success_line(self):
        for step in ("dev two-worker, cold-cache, cross-PR and fault acceptance",
                     "release and folded-group two-worker and fault acceptance"):
            with self.subTest(step=step), tempfile.TemporaryDirectory() as directory:
                _, env = self.shell_fixture(Path(directory), code=23)
                done = self.run_shell(PLATFORM, step, env)
                self.assertEqual(done.returncode, 23, done.stdout + done.stderr)

    def test_protocol_handoff_verifies_stage1_before_retaining_its_provenance(self):
        for code in (0, 23):
            with self.subTest(identity_exit=code), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                _, env = self.shell_fixture(root, code=code)
                binaries = root / "protocol-binaries"
                binaries.mkdir()
                manifest = root / identity.MANIFEST
                manifest.write_text('{"identity": "Stage 1 handoff fixture"}\n')
                with tarfile.open(binaries / "blaze.tar", "w") as archive:
                    archive.add(manifest, arcname=identity.MANIFEST)
                done = self.run_shell(PLATFORM, "verify the protocol test Stage 1 handoff", env)
                self.assertEqual(done.returncode, code, done.stdout + done.stderr)
                self.assertEqual(json.loads((root / "identity-input.json").read_text()),
                                 {"command": "verify", "binaries": str(binaries)})
                retained = root / "protocol-evidence/stage1-build.json"
                self.assertEqual(retained.exists(), code == 0)
                if code == 0:
                    self.assertEqual(retained.read_bytes(), manifest.read_bytes())

    def test_protocol_tests_use_stage1_for_all_targets_and_preserve_failure(self):
        for code in (0, 23):
            with self.subTest(test_exit=code), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                _, env = self.shell_fixture(root)
                evidence = root / "protocol-evidence"
                evidence.mkdir()
                binaries = root / "protocol-binaries"
                binaries.mkdir()
                blaze = binaries / "blaze"
                blaze.write_text(
                    "#!/bin/sh\n"
                    'printf \'%s\\n\' "$0" "$@" > "$RUNNER_TEMP/protocol-evidence/invocation.txt"\n'
                    f"printf '%s\\n' 'protocol test fixture exit {code}'\nexit {code}\n")
                blaze.chmod(0o700)
                # A return to the PATH's seed must fail even if the seed reports success:
                # B0 cannot compile these same-name lib/bin integration test targets.
                seed = root / "commands/blaze"
                seed.write_text("#!/bin/sh\nprintf '%s\\n' 'seed used instead of Stage 1'\nexit 0\n")
                seed.chmod(0o700)
                done = self.run_shell(PLATFORM, "distributed protocol and carrier tests with Stage 1", env)
                self.assertEqual(done.returncode, code, done.stdout + done.stderr)
                invocation = (evidence / "invocation.txt").read_text().splitlines()
                self.assertEqual(invocation[0], str(blaze))
                args = invocation[1:]
                self.assertEqual(args[:2], ["--ignore-pin", "test"])
                packages = [args[i + 1] for i, arg in enumerate(args) if arg == "-p"]
                self.assertCountEqual(packages, ["blaze-wire", "blaze-dist", "blaze-dist-local", "blaze-serve"])
                for flag in ("--release", "--locked", "--lib", "--tests", "--no-fail-fast"):
                    self.assertIn(flag, args)
                for filter_or_no_run in ("--no-run", "--test", "--"):
                    self.assertNotIn(filter_or_no_run, args)
                self.assertIn(f"protocol test fixture exit {code}", (evidence / "tests.log").read_text())

    def test_protocol_receipt_requires_completed_tests_log_and_unchanged_stage1(self):
        cases = [("success", "test output\n", 0, True),
                 ("success", "test output\n", 23, False),
                 ("success", None, 0, False), ("success", "", 0, False)]
        cases += [(outcome, "test output\n", 0, False)
                  for outcome in ("", "failure", "cancelled", "skipped")]
        for outcome, log, code, wanted in cases:
            with self.subTest(outcome=outcome, log=log, identity_exit=code), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                _, env = self.shell_fixture(root, code=code)
                env["PROTOCOL_TEST_OUTCOME"] = outcome
                evidence = root / "protocol-evidence"
                evidence.mkdir()
                if log is not None:
                    (evidence / "tests.log").write_text(log)
                done = self.run_shell(PLATFORM, "protocol tests completed with unchanged Stage 1 and source", env)
                self.assertEqual(done.returncode == 0, wanted, done.stdout + done.stderr)
                output = root / "output"
                self.assertEqual(output.read_text() if output.exists() else "", "accepted=true\n" if wanted else "")
                if outcome == "success" and log:
                    self.assertEqual(json.loads((root / "identity-input.json").read_text()),
                                     {"command": "verify", "binaries": str(root / "protocol-binaries")})

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
