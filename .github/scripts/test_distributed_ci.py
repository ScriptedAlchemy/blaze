#!/usr/bin/env python3
"""Hold the real CI shell to exact source identities and complete acceptance receipts.

Executable fixtures only test the harness and refusal paths; real compiler acceptance is
owned by the required hosted source and production jobs.
"""
import contextlib
import copy
import importlib.util
import io
import json
import os
from pathlib import Path
import shlex
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

import dist_build_identity as identity

ROOT = Path(__file__).resolve().parents[2]
MAIN = ROOT / ".github/workflows/blaze.yaml"
CONTROLS = (ROOT / ".github/distributed-controls.txt").read_text().splitlines()
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
        for stem in ("blaze", "blaze-launcher", "blaze-fact-shim", "blaze-dist-local", "blaze-serve"):
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
    def run_shell(self, name, env):
        return subprocess.run([*shell_command(MAIN, name), "-c", shell_step(MAIN, name)],
                              cwd=ROOT, env={**os.environ, **env}, text=True,
                              capture_output=True, timeout=30)

    def needs(self, tier="gate"):
        jobs = {name: {"result":"success", "outputs":{}} for name in
                ("changes","scripts","workflows","lean","cargo","distributed-core",
                 "source-compiler","production-acceptance")}
        jobs["changes"]["outputs"]["tier"] = tier
        jobs["source-compiler"]["outputs"]["source_smoke"] = "true"
        jobs["production-acceptance"]["outputs"] = {
            name:"true" for name in ("distributed_acceptance","forkfixes_acceptance","protocol_tests")}
        if tier == "none":
            for name in ("cargo","source-compiler","production-acceptance"):
                jobs[name] = {"result":"skipped", "outputs":{}}
        return jobs

    def test_aggregate_requires_actual_jobs_and_each_real_receipt(self):
        for tier in ("none","gate","warm"):
            base = self.needs(tier)
            self.assertEqual(self.run_shell("every required check passed", {"NEEDS":json.dumps(base)}).returncode, 0)
            for job, value in base.items():
                if value["result"] != "success":
                    continue
                for outcome in (None, "", "skipped", "failure", "cancelled"):
                    with self.subTest(tier=tier, job=job, outcome=outcome):
                        changed = copy.deepcopy(base)
                        if outcome is None:
                            del changed[job]
                        else:
                            changed[job]["result"] = outcome
                        done = self.run_shell("every required check passed", {"NEEDS":json.dumps(changed)})
                        self.assertNotEqual(done.returncode, 0, done.stdout + done.stderr)
        for job in ("source-compiler", "production-acceptance"):
            for name in self.needs()[job]["outputs"]:
                for value in (None, "", "false", True):
                    changed = self.needs()
                    changed[job]["outputs"][name] = value
                    self.assertNotEqual(self.run_shell("every required check passed", {"NEEDS":json.dumps(changed)}).returncode, 0)

    def fixture(self, root):
        commands = root / "commands"
        commands.mkdir()
        python = commands / "python3"
        python.write_text(
            "#!/bin/sh\ncase \"$1\" in\n"
            " .github/scripts/dist_build_identity.py) exit \"${IDENTITY_EXIT:-0}\" ;;\n"
            " harness/distv2.py) case \"$2\" in */dev-input.json) exit \"$DEV_EXIT\" ;;"
            " */release-input.json) exit \"$RELEASE_EXIT\" ;; *) exit 98 ;; esac ;;\n"
            " harness/blaze_cli.py) printf '%s\\n' \"${SMOKE_SAYS:-source CLI: every check passed}\"; exit \"${SMOKE_EXIT:-0}\" ;;\n"
            f" *) exec {shlex.quote(sys.executable)} \"$@\" ;;\nesac\n")
        python.chmod(0o700)
        (root / "dist-evidence").mkdir()
        (root / "source-evidence").mkdir()
        return {"RUNNER_TEMP":str(root), "GITHUB_OUTPUT":str(root / "output"),
                "GITHUB_WORKSPACE":str(ROOT), "PATH":str(commands) + os.pathsep + os.environ["PATH"]}

    def test_partial_library_checks_are_independent_and_cover_every_active_target(self):
        text = MAIN.read_text()
        header = text.split("  distributed-core:\n",1)[1].split("    steps:\n",1)[0]
        self.assertIn("    needs: [workflows]\n", header)
        self.assertNotIn("    if:", header)
        body = shell_step(MAIN,"distributed library models and planted controls with pinned Cargo")
        self.assertIn("--all-targets", body)
        self.assertIn("--no-fail-fast", body)
        for name in ("blaze-dist","blaze-dist-local","blaze-wire","blaze-serve"):
            self.assertIn("-p " + name, body)
        self.assertIn("done < .github/distributed-controls.txt", body)
        self.assertGreater(len(CONTROLS), 70)
        self.assertEqual(len(CONTROLS),len(set(CONTROLS)))
        self.assertIn("--workspace --exclude blaze-std --all-targets",text)

    def test_native_controls_must_execute_and_pipeline_failure_is_not_hidden_by_tee(self):
        for omit, code in ((None,0),(CONTROLS[0],0),(CONTROLS[-1],0),(None,23),("all",0)):
            with self.subTest(omit=omit,code=code), tempfile.TemporaryDirectory() as directory:
                root=Path(directory);env=self.fixture(root)
                reported=[] if omit == "all" else [name for name in CONTROLS if name != omit]
                log=root/"tests.txt"
                log.write_text("".join(f"test tests::{name} ... ok\n" for name in reported))
                for name in ("cargo","rustc"):
                    program=root/"commands"/name
                    program.write_text('#!/bin/sh\nif [ "$2" = test ]; then cat "$NATIVE_LOG"; exit "$NATIVE_EXIT"; fi\nexit 0\n')
                    program.chmod(0o700)
                done=self.run_shell("distributed library models and planted controls with pinned Cargo",
                                    {**env,"NATIVE_LOG":str(log),"NATIVE_EXIT":str(code)})
                self.assertEqual(done.returncode == 0, omit is None and code == 0, done.stdout+done.stderr)

    def test_owner_field_bound_receipt_requires_its_executed_case_and_cargo_success(self):
        case = "suite::instance_holes::owner_field_bounds_select_only_the_matching_impl"
        passed = f"test {case} ... ok\n"
        for log, code in ((passed, 0), ("running 0 tests\n", 0),
                          (passed.replace(case, "unrelated"), 0), (passed, 23),
                          (passed.replace("ok", "FAILED"), 0)):
            with self.subTest(log=log, code=code), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                env = self.fixture(root)
                saved = root / "tests.txt"
                saved.write_text(log)
                cargo = root / "commands/cargo"
                cargo.write_text(
                    '#!/bin/sh\nprintf "%s\\n" "$@" > "$CASE_ARGV"\n'
                    'cat "$CASE_LOG"\nexit "$CASE_EXIT"\n')
                cargo.chmod(0o700)
                done = self.run_shell("owner field-bound instance selection", {
                    **env, "CASE_ARGV":str(root / "argv.txt"), "CASE_LOG":str(saved), "CASE_EXIT":str(code),
                })
                wanted = log == passed and code == 0
                self.assertEqual(done.returncode == 0, wanted, done.stdout + done.stderr)
                self.assertEqual((root / "argv.txt").read_text().splitlines(), [
                    "+" + (ROOT / "pins/toolchain").read_text().strip(), "test", "--locked",
                    "-p", "blaze-typeck", "--lib", case, "--", "--exact", "--nocapture",
                ])
                self.assertEqual(int((root / "owner-field-bound-evidence/exit-code.txt").read_text()),
                                 0 if wanted else code or 1)

    def test_cabi_receipt_requires_both_emitters_all_cases_and_original_failures(self):
        cases = (
            "cabi::tests::classes_follow_the_fundamental_types_and_homogeneous_aggregates",
            "cabi::tests::sixteen_byte_alignment_rounds_to_an_even_register",
            "cabi::tests::i128_pairs_fit_or_spill_whole_and_return_in_x0_x1",
            "tests::cabi_i128::c_i128_signatures_keep_wide_scalars",
            "tests::cabi_i128::native::c_i128_register_pairs_match_host_abi",
            "tests::cabi_i128::native::c_i128_last_register_pair_matches_host_abi",
            "tests::cabi_i128::native::c_i128_spilled_pair_matches_host_abi",
            "tests::cabi_i128::native::c_i128_aligned_stack_pair_matches_host_abi",
            "sota::isel::abi_tests::i128_placement_uses_the_target_pair_rule_and_spills_whole",
        )
        for omitted, build_code, emit_code, failed in (
            (None, 0, 0, False), (cases[0], 0, 0, False), (cases[-1], 0, 0, False),
            ("all", 0, 0, False), (None, 0, 0, True), (None, 23, 0, False),
            (None, 0, 37, False), (None, 23, 37, False),
        ):
            with self.subTest(omitted=omitted, build=build_code, emit=emit_code, failed=failed), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                env = self.fixture(root)
                for package, names in (("build", cases[:-1]), ("emit", cases[-1:])):
                    log = "".join(f"test {name} ... ok\n" for name in names if omitted != "all" and name != omitted)
                    (root / f"{package}.log").write_text(log.replace(" ... ok", " ... FAILED", 1) if failed else log)
                cargo = root / "commands/cargo"
                cargo.write_text(
                    '#!/bin/sh\nprintf "%s\\n" "$@" > "$ABI_TRACE/$5.argv"\n'
                    'case "$5" in\n'
                    ' blaze-build) cat "$ABI_BUILD_LOG"; exit "$ABI_BUILD_EXIT" ;;\n'
                    ' blaze-emit) cat "$ABI_EMIT_LOG"; exit "$ABI_EMIT_EXIT" ;;\n'
                    ' *) exit 93 ;;\nesac\n')
                cargo.chmod(0o700)
                done = self.run_shell("source C ABI integer pairs through both emitters", {
                    **env, "ABI_TRACE":str(root), "ABI_BUILD_LOG":str(root / "build.log"),
                    "ABI_EMIT_LOG":str(root / "emit.log"), "ABI_BUILD_EXIT":str(build_code),
                    "ABI_EMIT_EXIT":str(emit_code),
                })
                wanted = build_code or emit_code or (1 if omitted or failed else 0)
                self.assertEqual(done.returncode, wanted, done.stdout + done.stderr)
                pin = "+" + (ROOT / "pins/toolchain").read_text().strip()
                self.assertEqual((root / "blaze-build.argv").read_text().splitlines(), [
                    pin, "test", "--locked", "-p", "blaze-build", "--lib", "cabi", "--", "--nocapture",
                ])
                self.assertEqual((root / "blaze-emit.argv").read_text().splitlines(), [
                    pin, "test", "--locked", "-p", "blaze-emit", "--lib", cases[-1], "--", "--exact", "--nocapture",
                ])
                self.assertEqual(int((root / "cabi-integer-evidence/exit-code.txt").read_text()), wanted)

    def test_source_smoke_failure_or_absent_completion_cannot_issue_receipt(self):
        for code, said, identity_code in ((0,"source CLI: every check passed",0),
                                          (23,"source CLI: every check passed",0),
                                          (0,"cold only passed",0),
                                          (0,"source CLI: every check passed",7)):
            with self.subTest(code=code,said=said,identity=identity_code), tempfile.TemporaryDirectory() as directory:
                root=Path(directory);env=self.fixture(root)
                done=self.run_shell("cold warm edited and exported hello with the source CLI",
                                    {**env,"SMOKE_EXIT":str(code),"SMOKE_SAYS":said,"IDENTITY_EXIT":str(identity_code)})
                wanted=code == 0 and identity_code == 0 and said == "source CLI: every check passed"
                self.assertEqual(done.returncode == 0,wanted,done.stdout+done.stderr)
                output=root/"output"
                self.assertEqual(output.read_text() if output.exists() else "", "accepted=true\n" if wanted else "")

    def test_fault_profiles_both_run_and_preserve_the_first_failure(self):
        for dev, release in ((0, 0), (19, 0), (0, 23), (19, 23)):
            with self.subTest(dev=dev, release=release), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                env = {**self.fixture(root), "DEV_EXIT": str(dev), "RELEASE_EXIT": str(release),
                       "BLAZE_SCRIPTS": str(root / "commands")}
                done = self.run_shell("full distributed worker and fault acceptance", env)
                self.assertEqual(done.returncode, dev or release, done.stdout + done.stderr)
                for profile in ("dev", "release"):
                    self.assertTrue((root / "dist-evidence" / f"{profile}.log").is_file())

    def test_every_full_fault_profile_and_unchanged_source_is_required(self):
        for dev,release,identity_code in ((PASSED,PASSED,0),(None,PASSED,0),
                                          (PASSED,"distv2: non-fault checks passed\n",0),
                                          (PASSED,PASSED,9)):
            with self.subTest(dev=dev,release=release,identity=identity_code), tempfile.TemporaryDirectory() as directory:
                root=Path(directory);env=self.fixture(root)
                for name,value in (("dev",dev),("release",release)):
                    if value is not None:
                        (root/f"dist-evidence/{name}.log").write_text(value)
                done=self.run_shell("both complete fault runs used unchanged binaries and source",{**env,"IDENTITY_EXIT":str(identity_code)})
                wanted=dev == release == PASSED and identity_code == 0
                self.assertEqual(done.returncode == 0,wanted,done.stdout+done.stderr)
                output=root/"output"
                self.assertEqual(output.read_text() if output.exists() else "", "accepted=true\n" if wanted else "")

    def test_source_protocol_needs_all_causal_cases_actual_launches_and_termination(self):
        packages=["blaze-dist","blaze-dist-local","blaze-wire","blaze-serve"]
        for plant in (None,"missing-control","reused","filtered","not-built","empty-package",
                      "failed-test","old-schema","old-scope","missing-command-pid","timed-out",
                      "bad-command","mismatched-command","missing-mode","lost-trace","changed-binary"):
            with self.subTest(plant=plant), tempfile.TemporaryDirectory() as directory:
                root=Path(directory);env=self.fixture(root)
                out=root/"dist-evidence/exec";out.mkdir()
                commands=[{"name":name,"pid":100+at,"returncode":0,"timed_out":False} for at,name in enumerate(packages)]
                if plant == "missing-command-pid": commands[0].pop("pid")
                elif plant == "timed-out": commands[0]["timed_out"]=True
                elif plant == "bad-command": commands[0]["returncode"]=23
                scope="linux-launched-descendants-through-blaze-stop" if plant == "old-scope" else "linux-command-descendants-through-exit"
                (out/"result.json").write_text(json.dumps({"schema":1 if plant == "old-schema" else 2,
                    "passed":plant != "lost-trace","returncode":0,"command_status":0,"scope":scope,
                    "environment":{} if plant == "missing-mode" else {"BLAZE_NO_DAEMON":"1"},
                    "commands":[] if plant == "mismatched-command" else commands}))
                (out/"supervisor.json").write_text(json.dumps({"schema":2,"scope":scope,"complete":True,
                    "commands":commands}))
                for at,package in enumerate(packages):
                    names=CONTROLS[at::len(packages)]
                    if at == 0 and plant == "missing-control": names=names[1:]
                    if at == 0 and plant == "empty-package": names=[]
                    tests=[{"event":"test","path":"tests::"+name,"outcome":"passed","ran_because":"run_all","failure":None,"basis":None} for name in names]
                    summary={"event":"summary","result":"ok","ran":len(names),"passed":len(names),"failed":0,
                             "reused":0,"filtered_out":0,"flaky":0}
                    if at == 0:
                        if plant == "reused": tests[0]["ran_because"]="reused"
                        elif plant == "filtered": summary["filtered_out"]=1
                        elif plant == "not-built": summary["not_built"]=1
                        elif plant == "failed-test": tests[0]["outcome"]="failed"
                    (out/(package+".log")).write_text("\n".join(json.dumps(row) for row in [*tests,summary])+"\n")
                done=self.run_shell("complete source protocol receipt with unchanged executables",
                                    {**env,"IDENTITY_EXIT":"9" if plant == "changed-binary" else "0"})
                self.assertEqual(done.returncode == 0,plant is None,done.stdout+done.stderr)
                output=root/"output"
                self.assertEqual(output.read_text() if output.exists() else "", "accepted=true\n" if plant is None else "")


class WorkerBridgeTests(unittest.TestCase):
    def load_harness(self):
        sys.path.insert(0,str(ROOT/"harness"))
        spec=importlib.util.spec_from_file_location("worker_acceptance",ROOT/"harness/distv2.py")
        worker=importlib.util.module_from_spec(spec);sys.modules[spec.name]=worker;spec.loader.exec_module(worker)
        return worker

    def test_bootstrap_preparation_tracks_source_and_preserves_immutable_records(self):
        worker = self.load_harness()
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            builder = root / "builder"
            builder.write_bytes(b"explicit sandboxed builder")
            project = root / "project"
            for name in ("pm", "shapes"):
                (project / name).mkdir(parents=True)
                (project / name / "lib.rs").write_text("original")
            (project / "Cargo.lock").write_text("fixture lock")
            (project / "Cargo.toml").write_text("[workspace]")
            given = worker.Input(blaze="unused", dist_local="unused", serve="unused",
                                 scratch=str(root / "run"), module_builder=str(builder))
            with patch.dict(os.environ, {"HOME": str(root / "empty-home")}), \
                    patch.object(worker, "free_port", return_value=1):
                run = worker.Run(given)
            calls = []

            def prepare(argv, **kwargs):
                supplied = json.loads(argv[1])
                script = "build_scripts" in supplied
                request = supplied["build_scripts"] if script else supplied
                self.assertIs(request["unsandboxed_build_scripts"], False)
                self.assertTrue(kwargs["start_new_session"])
                name = request["crates"][0]["name"]
                calls.append(name)
                kind = "build-script" if script else "proc-macro"
                key = Path(request["work"]).name
                record = Path(request["store"]) / kind / "modules" / key
                record.mkdir(parents=True)
                (record / "module.wasm").write_text((project / name / "lib.rs").read_text())
                class Completed:
                    def wait(self, timeout):
                        return 0
                return Completed()

            with patch.object(worker.subprocess, "Popen", side_effect=prepare):
                run.prepare_modules("a", project)
                run.prepare_modules("b", project)
                self.assertEqual(calls, ["pm", "shapes"])
                (project / "shapes/lib.rs").write_text("edited script package")
                run.prepare_modules("a", project)
                self.assertEqual(calls, ["pm", "shapes", "shapes"])
                records = list((run.root / "a/.blaze/v2/build-script/modules").glob("*/module.wasm"))
                self.assertEqual({p.read_text() for p in records}, {"original", "edited script package"})
                records[0].write_text("planted immutable-record corruption")
                with self.assertRaisesRegex(worker.Failed, "immutable bootstrap module differs"):
                    run.prepare_modules("a", project)

    def test_build_failure_includes_root_cause_and_final_diagnostic(self):
        worker = self.load_harness()
        root = "MacroModule: no build-script module of shapes"
        final = "the answer holds no CrateScope row for shapes"
        with self.assertRaises(worker.Failed) as failure:
            worker.require_build(1, f"{root}\n" + "x" * 10000 + f"\n{final}",
                                 "solo builds", Path("logs/solo.1.log"))
        message = str(failure.exception)
        for text in (root, final, "exit 1", "logs/solo.1.log", "see full build log"):
            self.assertIn(text, message)
        self.assertLess(len(message), 8500)

    def test_real_capability_reply_decides_whether_worker_acceptance_can_start(self):
        worker = self.load_harness()
        for code,stdout,works in ((0,'{"host":"aarch64-unknown-linux-gnu"}',True),
                                  (0,"Rust nightly-2026-11-11\n",False),
                                  (1,"unknown command",False),(0,'{"host":null}',False),
                                  (0,'{"host":"bad host"}',False)):
            with self.subTest(stdout=stdout),tempfile.TemporaryDirectory() as directory:
                out=Path(directory)
                with patch.object(worker.subprocess,"run",return_value=subprocess.CompletedProcess([],code,stdout,"")) as invoke:
                    if works:
                        self.assertEqual(worker.host_triple("/source/blaze",{},5,out),"aarch64-unknown-linux-gnu")
                    else:
                        with self.assertRaisesRegex(worker.Failed,"distributed worker protocol bridge"):
                            worker.host_triple("/source/blaze",{},5,out)
                self.assertEqual(invoke.call_args.args[0],["/source/blaze","toolchain","--json"])
                self.assertEqual(json.loads((out/"worker-protocol-probe.json").read_text())["stdout"],stdout)


if __name__ == "__main__":
    unittest.main()
