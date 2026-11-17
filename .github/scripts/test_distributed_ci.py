#!/usr/bin/env python3
"""CI refuses wrong build identities and incomplete distributed acceptance.

The small executables below are identity fixtures and shell command doubles. The protocol
shell uses the real kernel observer and strace; real Blaze acceptance belongs to the dependent job.
"""
import contextlib
import copy
import hashlib
import importlib.util
import io
import json
import os
from pathlib import Path
import shlex
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
PROTOCOL_STEP = "distributed protocol and carrier tests with Stage 1"
RELEASE_STEP = "daemon release and scheduler controls with Stage 1"
PROTOCOL_ACCEPTED_STEP = "protocol tests completed with unchanged Stage 1 and source"
EXECUTION_SCOPE = "linux-launched-descendants-through-blaze-stop"
RELEASE_CONTROLS = (
    "worker_release_keeps_the_loop_responsive_and_publication_fenced",
    "parked_successors_allow_release_with_one_work_slot",
    "cancellation_at_each_handoff_boundary_retires_the_build",
    "release_failure_and_panic_preserve_the_cause_and_keep_serving",
    "refused_release_uses_one_accounted_discard",
    "orphaned_worker_result_is_discarded_after_job_removal",
    "late_write_requires_another_final_release",
    "completion_preserves_later_pressure_finished_crates_and_end",
    "final_release_policy_requires_completion_after_every_late_write",
    "parking_an_unposted_task_after_cancel_or_failure_settles_without_a_token",
    "pending_history_uses_the_completed_handoff_outcome",
    "the_oracle_catches_inline_release",
    "the_oracle_catches_a_held_successor_grant",
    "the_oracle_catches_a_late_result_dropped_on_the_loop",
    "the_oracle_catches_keep_after_an_older_final_release",
    "the_oracle_catches_completion_erasing_later_pressure",
    "the_oracle_catches_completion_erasing_later_finished_crates",
    "the_oracle_catches_completion_erasing_a_later_end",
    "the_oracle_catches_reusing_a_final_plan_before_a_late_write",
    "the_oracle_catches_history_before_handoff_failure",
)
BUNDLE_MODELS = (
    "overlapping_downloads_wait_for_shared_fetches_and_keep_their_outcomes",
    "readable_bundle_values_keep_their_pending_durability_fence",
    "verified_manifests_keep_immutable_bindings_and_refuse_conflicts_atomically",
    "bundle_values_keep_their_manifest_bindings_across_local_and_shared_sources",
)
BUNDLE_CONTROLS = (
    "the_oracle_catches_a_download_ignoring_an_existing_fetch",
    "the_oracle_catches_a_shared_completion_consumed_by_one_download",
    "the_oracle_catches_a_readable_value_without_its_durability_fence",
    "the_oracle_catches_waiting_for_a_value_published_during_fetch_scheduling",
    "the_oracle_catches_a_lost_manifest_binding",
    "the_oracle_catches_a_partly_applied_conflicting_manifest",
    "the_oracle_catches_a_readable_value_from_another_manifest_record",
    "the_oracle_catches_a_racing_value_from_another_manifest_record",
    "the_oracle_catches_a_shared_fetch_from_another_manifest_record",
    "the_oracle_catches_an_old_fetch_outcome_delivered_after_a_new_manifest",
)
BUNDLE_CASES = BUNDLE_MODELS + BUNDLE_CONTROLS


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


def shell_command(workflow: Path, name: str) -> list[str]:
    """Match these workflows' declared Bash policy; unspecified Bash has no pipefail.

    An explicit step setting wins over the workflow default. The fixtures must not add
    pipefail when CI omitted it, or a failing compiler followed by a successful tee can pass.
    """
    source = workflow.read_text()
    lines = source.splitlines()
    at = lines.index(f"      - name: {name}") + 1
    declared = []
    while at < len(lines) and lines[at] != "        run: |":
        if lines[at].startswith("      - "):
            raise AssertionError(f"{name} has no shell body")
        if lines[at].startswith("        shell:"):
            declared.append(lines[at].split(":", 1)[1].strip())
        at += 1
    if not declared and "\ndefaults:\n  run:\n    shell: bash\n" in source.split("\njobs:\n", 1)[0]:
        declared = ["bash"]
    if not declared:
        return ["bash", "-e"]
    if declared != ["bash"]:
        raise AssertionError(f"{name}: unsupported shell policy {declared}")
    return ["bash", "--noprofile", "--norc", "-e", "-o", "pipefail"]


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
            [*shell_command(workflow, step), "-c", shell_step(workflow, step)],
            cwd=ROOT, env={**os.environ, **env}, text=True, capture_output=True, timeout=30)

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

    def shell_fixture(self, root, *, code=0, failure_harness=None):
        evidence = root / "dist-evidence"
        evidence.mkdir()
        (root / "blaze-fs").mkdir()
        commands = root / "commands"
        commands.mkdir()
        python = commands / "python3"
        harness_double = ""
        if failure_harness is not None:
            # The old tee/exit controls explicitly double their expensive harness. No
            # other script, especially the execution observer, may silently be stubbed.
            harness_double = (f"  {shlex.quote(failure_harness)})\n"
                              f"    printf '%s\\n' 'distv2: every check passed'; exit {code} ;;\n")
        python.write_text(
            "#!/bin/sh\n"
            'case "$1" in\n'
            "  harness/v2/exec_observer.py)\n"
            "    shift\n"
            f"    exec {shlex.quote(sys.executable)} {shlex.quote(str(ROOT / 'harness/v2/exec_observer.py'))} \"$@\" ;;\n"
            "  .github/scripts/dist_build_identity.py)\n"
            '    cat > "$RUNNER_TEMP/identity-input.json"\n'
            f"    exit {code} ;;\n"
            + harness_double
            + '  *) printf \'unexpected Python command: %s\\n\' "$1" >&2; exit 97 ;;\n'
            "esac\n")
        python.chmod(0o700)
        return evidence, {"RUNNER_TEMP": str(root), "GITHUB_OUTPUT": str(root / "output"),
                          "GITHUB_WORKSPACE": str(ROOT),
                          "PATH": str(commands) + os.pathsep + os.environ["PATH"]}

    def protocol_fixture(self, root, *, test_code=0, release_code=0, stop_code=0,
                         reported=RELEASE_CONTROLS, bundle_reported=BUNDLE_CASES):
        _, env = self.shell_fixture(root)
        evidence = root / "protocol-evidence"
        evidence.mkdir()
        binaries = root / "protocol-binaries"
        binaries.mkdir()
        blaze = binaries / "blaze"
        blaze.write_text(
            "#!/bin/sh\n"
            '[ "$1" = --ignore-pin ] || exit 91\n'
            'case "$2" in\n'
            '  stop) name=stop ;;\n'
            '  test) case " $* " in *" -p blaze-cargo "*) name=release ;; *) name=tests ;; esac ;;\n'
            '  *) exit 92 ;;\n'
            'esac\n'
            'printf \'%s\\n\' "$name" >> "$RUNNER_TEMP/protocol-evidence/execution-order.txt"\n'
            'printf \'%s\\n\' "$0" "$@" > "$RUNNER_TEMP/protocol-evidence/$name-invocation.txt"\n'
            'printf \'%s\\n\' "$BLAZE_TESTS" > "$RUNNER_TEMP/protocol-evidence/$name-mode.txt"\n'
            'printf \'%s\\n\' "$BLAZE_HOME" "$BLAZE_CACHE" > "$RUNNER_TEMP/protocol-evidence/$name-home.txt"\n'
            'if [ "$name" = stop ]; then printf \'%s\\n\' "shutdown fixture"; exit "$STOP_EXIT"; fi\n'
            '"$RUNNER_TEMP/protocol-binaries/blaze-engine" "$name" || exit $?\n'
            'if [ "$name" = tests ]; then cat "$PROTOCOL_OUTPUT"; exit "$PROTOCOL_EXIT"; fi\n'
            'cat "$RELEASE_OUTPUT"\nexit "$RELEASE_EXIT"\n')
        blaze.chmod(0o700)
        engine = binaries / "blaze-engine"
        engine.write_text(
            "#!/bin/sh\n"
            'printf \'%s\\n\' "$0" "$@" > "$RUNNER_TEMP/protocol-evidence/engine-$1-invocation.txt"\n'
            "exit 0\n")
        engine.chmod(0o700)
        # The seed succeeds deliberately. Selecting it must still fail the identity,
        # invocation and trace assertions; B0 cannot compile these integration targets.
        seed = root / "commands/blaze"
        seed.write_text(
            "#!/bin/sh\n"
            'printf \'%s\\n\' "$0" "$@" > "$RUNNER_TEMP/wrong-path-seed-used"\n'
            "printf '%s\\n' 'seed used instead of Stage 1'\nexit 0\n")
        seed.chmod(0o700)
        protocol_output = root / "test-output.txt"
        protocol_output.write_text(f"protocol test fixture exit {test_code}\n"
                                   + "".join(f"test tests::download::{name} ... ok\n" for name in bundle_reported))
        release_output = root / "release-output.txt"
        release_output.write_text(f"running {len(reported)} tests\n"
                                  + "".join(f"test serve::tests::release::{name} ... ok\n" for name in reported))
        # Read the actual executing step, not the later release-log validation step.
        header = PLATFORM.read_text().split(f"      - name: {PROTOCOL_STEP}\n", 1)[1].split("        run: |\n", 1)[0]
        mode = [line.split(":", 1)[1].strip().strip("\"'") for line in header.splitlines()
                if line.startswith("          BLAZE_TESTS:")]
        self.assertEqual(mode, ["verify"])
        env.update(BLAZE_TESTS=mode[0], PROTOCOL_OUTPUT=str(protocol_output), PROTOCOL_EXIT=str(test_code),
                   RELEASE_OUTPUT=str(release_output), RELEASE_EXIT=str(release_code), STOP_EXIT=str(stop_code),
                   BLAZE_HOME=str(root / "inherited-home"), BLAZE_CACHE=str(root / "inherited-cache"))
        return evidence, binaries, env

    def assert_protocol_evidence(self, root, *, test_code=0, release_code=0, stop_code=0):
        evidence = root / "protocol-evidence"
        binaries = root / "protocol-binaries"
        commands = [("tests", test_code)] + ([("release", release_code)] if test_code == 0 else [])
        expected_code = test_code or release_code or stop_code
        receipt = json.loads((evidence / "exec/result.json").read_text())
        self.assertEqual(receipt["schema"], 1)
        self.assertEqual(receipt["scope"], EXECUTION_SCOPE)
        self.assertIs(receipt["passed"], expected_code == 0)
        self.assertEqual(receipt["returncode"], expected_code)
        self.assertEqual(receipt["compiler"], {
            name: hashlib.sha256((binaries / name).read_bytes()).hexdigest() for name in ("blaze", "blaze-engine")})
        supervisor = json.loads((evidence / "exec/supervisor.json").read_text())
        self.assertEqual(supervisor["commands"], [{"name": name, "returncode": code} for name, code in commands])
        self.assertEqual(supervisor["stop"], stop_code)
        self.assertIs(supervisor["complete"], True)
        executions = [entry["path"] for entry in receipt["trace"]["executions"] if entry["succeeded"]]
        self.assertEqual(executions.count(str(binaries / "blaze")), len(commands) + 1)
        self.assertEqual(executions.count(str(binaries / "blaze-engine")), len(commands))
        self.assertFalse((root / "wrong-path-seed-used").exists())
        order = [name for name, _ in commands] + ["stop"]
        self.assertEqual((evidence / "execution-order.txt").read_text().splitlines(), order)
        for name in order:
            self.assertEqual((evidence / f"{name}-home.txt").read_text().splitlines(),
                             [str(root / "blaze-fs/protocol-tests"), str(root / "blaze-fs/protocol-tests/v2/store")])
            self.assertEqual((evidence / f"{name}-mode.txt").read_text(), "verify\n")
        self.assertEqual((evidence / "stop-invocation.txt").read_text().splitlines(),
                         [str(binaries / "blaze"), "--ignore-pin", "stop"])

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
            "reclamation_preserves_full_grants_reservations_and_worker_limits",
            "reclamation_refuses_invalid_shapes_and_respects_actual_machine_bounds",
            "parked_unposted_work_releases_the_lane_without_losing_its_continuation",
            "parked_work_unblocks_reclamation_with_one_worker_and_no_budget",
            "a_reclamation_hold_is_respected_without_a_configured_budget",
        ] + list(BUNDLE_MODELS)
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
            "the_oracle_catches_reclamation_treated_as_ordinary_work",
            "the_oracle_catches_a_reclamation_grant_clamped_to_the_pressure_cap",
            "the_oracle_catches_a_lost_held_compile_reservation",
            "the_oracle_catches_positive_work_overlapping_reclamation",
            "the_oracle_catches_reclamation_spending_unreserved_machine_room",
            "the_oracle_catches_a_park_that_eagerly_readmits_unposted_work",
            "the_oracle_catches_a_stale_continuation_replaced_with_the_current_token",
            "the_oracle_catches_a_rejected_reclamation_prefix_entering_the_plane",
            "the_oracle_catches_a_reclamation_hold_bypassed_without_a_budget",
        ] + list(BUNDLE_CONTROLS)
        tls_cases = [
            "tls_bundle_publication_downloads_into_an_empty_local_store",
            "tls_bundle_refuses_an_untrusted_certificate_or_wrong_server_name",
            "tls_bundle_refuses_a_manifest_with_an_unpinned_or_damaged_signature",
            "tls_values_need_a_pinned_record_or_a_matching_trusted_manifest",
            "tls_bundle_refuses_a_value_not_matching_its_verified_manifest",
        ]
        names = models + controls + tls_cases
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
        cases += [(f"missing TLS case {name}", [item for item in names if item != name], 0, 1)
                  for name in tls_cases]
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
                                              "--lib", "-p", "blaze-dist", "-p", "blaze-dist-local",
                                              "-p", "blaze-graph", "-p", "blaze-global", "--no-fail-fast"])
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
                _, env = self.shell_fixture(Path(directory), code=23, failure_harness="harness/distv2.py")
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
        for test_code, release_code, stop_code in ((0, 0, 0), (23, 0, 0), (0, 24, 0),
                                                   (0, 0, 25), (23, 0, 25), (0, 24, 25)):
            with self.subTest(test_exit=test_code, release_exit=release_code,
                              stop_exit=stop_code), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                evidence, binaries, env = self.protocol_fixture(
                    root, test_code=test_code, release_code=release_code, stop_code=stop_code)
                done = self.run_shell(PLATFORM, PROTOCOL_STEP, env)
                self.assertEqual(done.returncode, test_code or release_code or stop_code, done.stdout + done.stderr)
                self.assert_protocol_evidence(root, test_code=test_code, release_code=release_code, stop_code=stop_code)
                # Exact argv preserves every lib/integration target and permits no
                # positional filter, --test selection, --no-run or skipped package.
                self.assertEqual((evidence / "tests-invocation.txt").read_text().splitlines(),
                                 [str(binaries / "blaze"), "--ignore-pin", "test", "--release", "--locked",
                                  "-p", "blaze-wire", "-p", "blaze-dist", "-p", "blaze-dist-local",
                                  "-p", "blaze-serve", "-p", "blaze-global", "-p", "blaze-graph",
                                  "--lib", "--tests", "--no-fail-fast"])
                self.assertIn(f"protocol test fixture exit {test_code}", (evidence / "exec/tests.log").read_text())
                if test_code:
                    self.assertFalse((evidence / "exec/release.log").exists())
                    self.assertFalse((evidence / "release-invocation.txt").exists())
                else:
                    self.assertEqual((evidence / "release-invocation.txt").read_text().splitlines(),
                                     [str(binaries / "blaze"), "--ignore-pin", "test", "--release", "--locked", "--lib",
                                      "-p", "blaze-cargo", "--no-fail-fast", "--", "serve::"])
                    self.assertEqual((evidence / "exec/release.log").read_bytes(), Path(env["RELEASE_OUTPUT"]).read_bytes())

    def test_protocol_receipt_requires_completed_logs_kernel_evidence_and_unchanged_stage1(self):
        receipt = {"schema": 1, "scope": EXECUTION_SCOPE, "passed": True, "returncode": 0}
        valid = json.dumps(receipt)
        cases = [("success", "test output\n", "success", "release output\n", valid, 0, True),
                 ("success", "test output\n", "success", "release output\n", valid, 23, False),
                 ("success", None, "success", "release output\n", valid, 0, False),
                 ("success", "", "success", "release output\n", valid, 0, False),
                 ("success", "test output\n", "success", None, valid, 0, False),
                 ("success", "test output\n", "success", "", valid, 0, False)]
        cases += [(outcome, "test output\n", "success", "release output\n", valid, 0, False)
                  for outcome in ("", "failure", "cancelled", "skipped")]
        cases += [("success", "test output\n", outcome, "release output\n", valid, 0, False)
                  for outcome in ("", "failure", "cancelled", "skipped")]
        invalid_receipts = [None, "", "{", "null", "[]", "{}"]
        for name, values in (("schema", (None, True, 2, "1")),
                             ("scope", (None, "", "client-only")),
                             ("passed", (None, False, 1, "true")),
                             ("returncode", (None, True, 23, "0"))):
            omitted = dict(receipt)
            omitted.pop(name)
            invalid_receipts.append(json.dumps(omitted))
            for value in values:
                invalid_receipts.append(json.dumps({**receipt, name: value}))
        cases += [("success", "test output\n", "success", "release output\n", raw, 0, False)
                  for raw in invalid_receipts]
        for outcome, log, release_outcome, release_log, raw, code, wanted in cases:
            with self.subTest(outcome=outcome, log=log, release=release_outcome,
                              release_log=release_log, receipt=raw,
                              identity_exit=code), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                _, env = self.shell_fixture(root, code=code)
                env["PROTOCOL_TEST_OUTCOME"] = outcome
                env["RELEASE_TEST_OUTCOME"] = release_outcome
                evidence = root / "protocol-evidence/exec"
                evidence.mkdir(parents=True)
                if log is not None:
                    (evidence / "tests.log").write_text(log)
                if release_log is not None:
                    (evidence / "release.log").write_text(release_log)
                if raw is not None:
                    (evidence / "result.json").write_text(raw)
                done = self.run_shell(PLATFORM, PROTOCOL_ACCEPTED_STEP, env)
                self.assertEqual(done.returncode == 0, wanted, done.stdout + done.stderr)
                output = root / "output"
                self.assertEqual(output.read_text() if output.exists() else "", "accepted=true\n" if wanted else "")
                if outcome == "success" and log and release_outcome == "success" and release_log and raw == valid:
                    self.assertEqual(json.loads((root / "identity-input.json").read_text()),
                                     {"command": "verify", "binaries": str(root / "protocol-binaries")})
                else:
                    self.assertFalse((root / "identity-input.json").exists())

    def test_daemon_release_receipt_requires_stage1_and_every_executed_control(self):
        cases = [("complete", RELEASE_CONTROLS, 0, 0), ("zero tests", (), 0, 1)]
        cases += [(f"missing {name}", [item for item in RELEASE_CONTROLS if item != name], 0, 1)
                  for name in RELEASE_CONTROLS]
        cases.append(("compiler failed after success lines", RELEASE_CONTROLS, 23, 0))
        for label, reported, code, expected in cases:
            with self.subTest(case=label), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                evidence, binaries, env = self.protocol_fixture(root, release_code=code, reported=reported)
                protocol = self.run_shell(PLATFORM, PROTOCOL_STEP, env)
                self.assertEqual(protocol.returncode, code, protocol.stdout + protocol.stderr)
                done = self.run_shell(PLATFORM, RELEASE_STEP, env)
                self.assertEqual(done.returncode, expected, done.stdout + done.stderr)
                # The line check must not launch another compiler outside the trace.
                self.assert_protocol_evidence(root, release_code=code)
                self.assertEqual((evidence / "release-invocation.txt").read_text().splitlines(),
                                 [str(binaries / "blaze"), "--ignore-pin", "test", "--release", "--locked", "--lib",
                                  "-p", "blaze-cargo", "--no-fail-fast", "--", "serve::"])
                self.assertEqual((evidence / "exec/release.log").read_bytes(), Path(env["RELEASE_OUTPUT"]).read_bytes())
                if expected == 1:
                    missing = next(name for name in RELEASE_CONTROLS if name not in reported)
                    self.assertIn("missing executed daemon release control: " + missing, done.stdout)
                # Success lines cannot rescue a failed compiler or an incomplete set.
                env.update(PROTOCOL_TEST_OUTCOME="success" if protocol.returncode == 0 else "failure",
                           RELEASE_TEST_OUTCOME="success" if done.returncode == 0 else "failure")
                accepted = self.run_shell(PLATFORM, PROTOCOL_ACCEPTED_STEP, env)
                wanted = code == 0 and expected == 0
                self.assertEqual(accepted.returncode == 0, wanted, accepted.stdout + accepted.stderr)
                output = root / "output"
                self.assertEqual(output.read_text() if output.exists() else "", "accepted=true\n" if wanted else "")

    def test_protocol_receipt_requires_every_bundle_model_and_control_under_stage1(self):
        cases = [("complete", BUNDLE_CASES, 0), ("zero bundle tests", (), 1)]
        cases += [(f"missing {name}", [item for item in BUNDLE_CASES if item != name], 1)
                  for name in BUNDLE_CASES]
        for label, reported, expected in cases:
            with self.subTest(case=label), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                evidence, _, env = self.protocol_fixture(root, bundle_reported=reported)
                protocol = self.run_shell(PLATFORM, PROTOCOL_STEP, env)
                self.assertEqual(protocol.returncode, 0, protocol.stdout + protocol.stderr)
                checked = self.run_shell(PLATFORM, RELEASE_STEP, env)
                self.assertEqual(checked.returncode, expected, checked.stdout + checked.stderr)
                self.assert_protocol_evidence(root)
                self.assertEqual((evidence / "exec/tests.log").read_bytes(), Path(env["PROTOCOL_OUTPUT"]).read_bytes())
                if expected:
                    missing = next(name for name in BUNDLE_CASES if name not in reported)
                    self.assertIn("missing executed bundle control: " + missing, checked.stdout)
                env.update(PROTOCOL_TEST_OUTCOME="success",
                           RELEASE_TEST_OUTCOME="success" if checked.returncode == 0 else "failure")
                accepted = self.run_shell(PLATFORM, PROTOCOL_ACCEPTED_STEP, env)
                self.assertEqual(accepted.returncode == 0, expected == 0, accepted.stdout + accepted.stderr)
                output = root / "output"
                self.assertEqual(output.read_text() if output.exists() else "",
                                 "accepted=true\n" if expected == 0 else "")

    def test_fork_migration_harness_failure_reaches_ci(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            _, env = self.shell_fixture(root, code=23, failure_harness="harness/forkfixes.py")
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
