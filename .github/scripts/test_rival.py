#!/usr/bin/env python3
"""Rival's selection and verdict controls; synthetic compilers are never executed.

Use the real Side, typed output readers, edits, schedule, report and main. Only process,
network, source fetching and the filesystem clone probe are controlled at their boundaries.
"""
import contextlib
import copy
import io
import json
import os
from pathlib import Path
import socket
import subprocess
import sys
import tempfile
import unittest
import urllib.request
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "harness"))
import rival  # noqa: E402

PROJECT = "ripgrep"
ORIGINAL = "fn answer() -> i32 { 1 }\nfn main() { let _ = answer(); }\n"
SPEC = dict(repo="fixture/project", rev="c" * 40, build=[], run=[], test=[], edits={
    "body": [("src/main.rs", "{ 1 }", "{ 1 + 0 }")],
    "signature": [("src/main.rs", "fn answer()", "fn answer(_: ())"),
                  ("src/main.rs", "answer();", "answer(());")],
})
HELP = "status      What the daemon is doing (--json)\ntoolchain   Download the toolchain (--json)\n"


def finished(*, producer="daemon", success=True):
    return json.dumps(dict(reason="build-finished", success=success, profile="dev",
                           served=producer, took_ms=1, build_id=1, units=None, links=None),
                      separators=(",", ":")) + "\n"


class RivalTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name).resolve()
        self.source = self.root / "source"
        (self.source / "src").mkdir(parents=True)
        (self.source / "src/main.rs").write_text(ORIGINAL)
        (self.source / "Cargo.toml").write_text('[package]\nname = "fixture"\nversion = "0.1.0"\n')
        self.arms = []
        for name, kind, companion, commit in (("ours", "blaze", "blaze-engine", "a"),
                                               ("astral", "astral", "ra-jit", "b")):
            directory = self.root / "bin" / name
            directory.mkdir(parents=True)
            for binary in ("blaze", companion):
                path = directory / binary
                path.write_text(f"synthetic {name}/{binary}; never executed\n")
                path.chmod(0o700)
            self.arms.append(dict(name=name, kind=kind, bin=str(directory), commit=commit * 40))
        self.base = dict(arms=self.arms, projects=[PROJECT], work=str(self.root / "work"),
                         out=str(self.root / "out"), runs=2, tests=True, machine="fixture")
        self.calls, self.probes = [], []
        self.mode = "good"
        stack = contextlib.ExitStack()
        self.addCleanup(stack.close)
        for owner, name in ((subprocess, "run"), (subprocess, "Popen"), (os, "system"),
                            (socket, "create_connection"), (urllib.request, "urlopen")):
            stack.enter_context(patch.object(owner, name, side_effect=AssertionError(
                f"rival controls must not call {owner.__name__}.{name}")))
        stack.enter_context(patch.object(rival, "spec_of_all", return_value={PROJECT: SPEC}))
        stack.enter_context(patch.object(rival.platform, "platform", return_value="fixture"))
        stack.enter_context(patch.object(rival.clone_out, "selftest", return_value=None))
        stack.enter_context(patch.object(rival, "fetch", return_value=str(self.source)))

    def probe(self, argv, **kwargs):
        name = Path(argv[0]).parent.name
        self.probes.append((name, argv, kwargs))
        if argv[1:] == ["--help"]:
            text = ("build  Cargo once, then the daemon\ntoolchain  Download the Rust toolchain\n"
                    if self.mode == "legacy" and name == "astral" else HELP)
        elif argv[1:] == ["--ignore-pin", "toolchain", "--json"]:
            if self.mode == "legacy" and name == "astral":
                self.fail("the legacy fork's dispatch would run Cargo; only --help is safe")
            text = ("blaze builds with human-only-output" if self.mode == "bad-toolchain" and name == "astral"
                    else json.dumps(dict(name="fixture", nightly="nightly-fixture", rustc=f"/{name}/rustc",
                                         release="1.fixture", commit="d" * 40, host="fixture-linux",
                                         sysroot=f"/{name}/sysroot")))
        else:
            self.fail(f"unexpected preflight process: {argv}")
        return subprocess.CompletedProcess(argv, 0, text, "")

    def timed(self, argv, cwd, env, timeout=7200):
        name, command = Path(argv[0]).parent.name, argv[2]
        self.assertEqual(argv[1], "--ignore-pin")
        self.assertEqual(argv[0], str(self.root / "bin" / name / "blaze"))
        text = Path(cwd, "src/main.rs").read_text()
        earlier = sum(it[0] == name and it[1] == command for it in self.calls)
        self.calls.append((name, command, text, dict(env), list(argv)))
        code, output, stderr = 0, finished(), ""
        if command == "stop":
            if self.mode == "cleanup-mutation" and name == "astral":
                Path(argv[0]).with_name("ra-jit").write_text("changed while stopping\n")
            return 0.01, subprocess.CompletedProcess(argv, 9 if self.mode == "failed-cleanup" else 0, "", "stop failed")
        self.assertIn(rival.blaze_cli.MACHINE, argv)
        if command == "build" and name == "astral" and earlier == 0:
            if self.mode in ("missing-producer", "failed-cleanup"):
                output, stderr = "", "[ra-jit] built successfully\n"
            elif self.mode == "other-producer":
                output = finished(producer="in-process")
            elif self.mode == "malformed-producer":
                output = '{"reason":"build-finished","success":"yes"}\n'
        if command == "run":
            # Different arm outputs are intentional: each arm is held to its own cold run.
            output += f"{name}'s program\n"
            if name == "astral" and ((self.mode == "wrong-edit" and earlier == 1)
                                     or (self.mode == "wrong-reversal" and earlier == 2)):
                output += "stale program output\n"
        elif command == "test":
            if self.mode == "empty-tests" and name == "astral":
                pass
            elif self.mode == "first-test-fails" and name == "astral" and earlier == 0:
                code = 101
                output += ("test own_test ... FAILED\nfailures:\n    own_test\n\n"
                           "test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out\n")
            else:
                output += ("test own_test ... ok\n"
                           "test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out\n")
        return 0.2, subprocess.CompletedProcess(argv, code, output, stderr)

    def invoke(self, *, mode="good", suffix="", **changes):
        self.mode = mode
        self.calls, self.probes = [], []
        given = dict(self.base, work=str(self.root / ("work" + suffix)),
                     out=str(self.root / ("out" + suffix)), **changes)
        with patch.object(sys, "argv", ["rival.py", json.dumps(given)]), \
                patch.object(subprocess, "run", side_effect=self.probe), \
                patch.object(rival.blaze_cli, "timed", side_effect=self.timed), \
                contextlib.redirect_stdout(io.StringIO()), self.assertRaises(SystemExit) as ended:
            rival.main()
        out = Path(given["out"])
        return (ended.exception.code, json.loads((out / "host.json").read_text()),
                json.loads((out / "raw" / f"{PROJECT}.json").read_text()), (out / "README.md").read_text())

    def test_typed_input_selects_two_real_arms_and_refuses_ambient_or_cargo_overrides(self):
        given, arms = rival.typed(json.dumps(self.base))
        self.assertEqual(given.runs, 2)
        self.assertEqual([arm.kind for arm in arms], ["blaze", "astral"])
        mutations = [dict(runs=True), dict(tests=1), dict(projects=[]), dict(projects=[PROJECT, PROJECT]),
                     dict(projects=[{}]), dict(action="unknown"), dict(compiler="ambient")]
        for mutation in mutations:
            with self.subTest(mutation=mutation), self.assertRaises(SystemExit):
                rival.typed(json.dumps(dict(self.base, **mutation)))
        for mutation in (dict(kind="cargo"), dict(env={"BLAZE_CLI_COMPILER": "ours"}),
                         dict(path=["ambient"]), dict(commit="main"), dict(name="../ours"),
                         dict(bin=self.arms[0]["bin"])):
            value = copy.deepcopy(self.base)
            value["arms"][1].update(mutation)
            with self.subTest(mutation=mutation), self.assertRaises(SystemExit):
                rival.typed(json.dumps(value))

    def test_real_side_selects_each_arm_and_isolates_all_placement_roots(self):
        poison = dict(BLAZE_CLI_COMPILER="/ambient/ours", BLAZE_HOME="/ambient/home", BLAZE_CACHE="/ambient/store",
                      XDG_CACHE_HOME="/ambient/cache", RUSTUP_TOOLCHAIN="ambient", CARGO="/ambient/cargo",
                      RUSTC="/ambient/rustc", CARGO_ENCODED_RUSTFLAGS="ambient")
        roots = []
        with patch.dict(os.environ, poison):
            for raw in self.arms:
                arm = rival.Arm(**raw)
                runner = rival.Runner(arm, str(self.root / arm.name), str(self.root / "placement"),
                                      PROJECT, SPEC, rival.Log(str(self.root / "commands.txt")))
                self.assertEqual(runner.side.argv("build")[:3], [str(Path(arm.bin, "blaze")), "--ignore-pin", "build"])
                for key in ("BLAZE_HOME", "BLAZE_CACHE", "XDG_CACHE_HOME", "CARGO_TARGET_DIR"):
                    self.assertNotIn("ambient", runner.env[key])
                    roots.append(runner.env[key])
                for key in ("RUSTUP_TOOLCHAIN", "CARGO", "RUSTC", "CARGO_ENCODED_RUSTFLAGS"):
                    self.assertNotIn(key, runner.env)
        self.assertEqual(len(set(roots)), len(roots))

    def test_an_external_launcher_cannot_leave_its_companions_outside_the_identity(self):
        arm = rival.Arm(**self.arms[1])
        selected = Path(arm.bin, "blaze")
        selected.unlink()
        selected.symlink_to(Path(self.arms[0]["bin"], "blaze"))
        with self.assertRaisesRegex(RuntimeError, "selected directory"):
            rival.compiler_identity(arm)
        with self.assertRaisesRegex(SystemExit, "different Blaze executables"):
            rival.typed(json.dumps(self.base))

    def test_legacy_help_refusal_is_retained_without_cargo_toolchain_builds_or_measurements(self):
        code, info, row, readme = self.invoke(mode="legacy")
        self.assertEqual(code, 1)
        self.assertEqual([argv[1:] for name, argv, _ in self.probes if name == "astral"], [["--help"]])
        self.assertEqual(self.calls, [])
        self.assertTrue(any("before toolchain setup or builds" in error for error in info["errors"]))
        self.assertTrue(all(not arm["phases"] and arm["failed"] for arm in row["arms"].values()))
        self.assertIn("Cargo once", (self.root / "out/raw/protocol-astral.txt").read_text())
        self.assertIn("Comparison refused; no ratios", readme)

    def test_malformed_toolchain_cannot_enter_measurement(self):
        code, info, _, _ = self.invoke(mode="bad-toolchain")
        self.assertEqual(code, 1)
        self.assertEqual(self.calls, [])
        self.assertTrue(any("not JSON" in error for error in info["errors"]))

    def test_real_main_measures_both_exact_compilers_with_rotating_own_oracles_and_cleanup(self):
        code, info, row, readme = self.invoke()
        self.assertEqual(code, 0, readme)
        self.assertEqual(rival.evidence_errors(info, [row]), [])
        builds = [name for name, command, *_ in self.calls if command == "build"]
        self.assertEqual(builds[:6], ["astral", "ours", "ours", "astral", "astral", "ours"])
        for name in ("ours", "astral"):
            own = [call for call in self.calls if call[0] == name]
            self.assertEqual([call[2] for call in own if call[1] == "run"],
                             [ORIGINAL, ORIGINAL.replace("{ 1 }", "{ 1 + 0 }"), ORIGINAL,
                              ORIGINAL.replace("fn answer()", "fn answer(_: ())").replace("answer();", "answer(());"), ORIGINAL])
            self.assertEqual(sum(call[1] == "test" for call in own), 2)
            self.assertEqual(own[-1][1], "stop")
            self.assertEqual(Path(self.root, "work", PROJECT, "ours", name, "src/main.rs").read_text(), ORIGINAL)
            self.assertEqual(row["arms"][name]["wrong"], [])
        self.assertTrue(all(arm["compiler"] == arm["compiler_after"] for arm in info["arms"]))
        self.assertIn("| 1.00x |", readme)

    def test_non_interleaved_arms_stop_before_the_other_begins(self):
        code, _, _, readme = self.invoke(interleave=False)
        self.assertEqual(code, 0, readme)
        names = [call[0] for call in self.calls]
        boundary = names.index("astral")
        self.assertTrue(all(name == "ours" for name in names[:boundary]))
        self.assertEqual(self.calls[boundary - 1][1], "stop")
        self.assertTrue(all(name == "astral" for name in names[boundary:]))

    def test_missing_malformed_or_other_producer_never_supplies_a_cold_sample(self):
        for mode in ("missing-producer", "malformed-producer", "other-producer"):
            with self.subTest(mode=mode):
                code, _, row, readme = self.invoke(mode=mode, suffix=mode)
                self.assertEqual(code, 1)
                self.assertNotIn("cold", row["arms"]["astral"]["phases"])
                self.assertTrue(row["arms"]["astral"]["failed"])
                self.assertEqual([name for name, command, *_ in self.calls if command == "stop"], ["ours", "astral"])
                self.assertIn("Comparison refused; no ratios", readme)

    def test_edit_reversal_and_both_test_verdicts_are_causal(self):
        for mode, expected in (("wrong-edit", "body:"), ("wrong-reversal", "body reversed:"),
                               ("first-test-fails", "tests round 1:"), ("empty-tests", "no test ran")):
            with self.subTest(mode=mode):
                code, _, row, readme = self.invoke(mode=mode, suffix=mode)
                self.assertEqual(code, 1)
                self.assertTrue(any(expected in wrong for wrong in row["arms"]["astral"]["wrong"]))
                self.assertIn("Comparison refused; no ratios", readme)
                if mode == "first-test-fails":
                    phases = row["arms"]["astral"]["phases"]
                    self.assertFalse(phases["tests_first"]["same"])
                    self.assertTrue(phases["tests"]["same"], "the second pass must not hide the first failure")

    def test_cleanup_failure_preserves_original_and_identity_is_rechecked_after_stop(self):
        code, _, row, _ = self.invoke(mode="failed-cleanup", suffix="failure")
        self.assertEqual(code, 1)
        why = row["arms"]["astral"]["failed"]
        self.assertIn("cold build was not produced", why)
        self.assertIn("cleanup:", why)
        code, info, _, _ = self.invoke(mode="cleanup-mutation", suffix="mutation")
        self.assertEqual(code, 1)
        self.assertTrue(any("before cleanup completed" in error for error in info["errors"]))

    def test_reused_or_unproven_placement_refuses_before_any_compiler_process(self):
        Path(self.base["work"]).mkdir()
        code, info, _, _ = self.invoke()
        self.assertEqual(code, 1)
        self.assertTrue(any("work already exists" in error for error in info["errors"]))
        self.assertEqual(self.probes, [])
        with patch.object(rival.clone_out, "selftest", side_effect=rival.clone_out.engine.Failed("no shared blocks")):
            code, info, _, _ = self.invoke(suffix="no-clone")
        self.assertEqual(code, 1)
        self.assertTrue(any("no shared blocks" in error for error in info["errors"]))
        self.assertEqual(self.probes, [])

    def test_report_refuses_partial_evidence_and_keeps_historical_cargo_labels(self):
        code, info, row, _ = self.invoke()
        self.assertEqual(code, 0)
        for label in ("cold", "no-op", "body", "signature", "tests_first", "tests"):
            value = copy.deepcopy(row)
            del value["arms"]["astral"]["phases"][label]
            with self.subTest(missing_phase=label):
                self.assertTrue(any(label in error for error in rival.evidence_errors(info, [value])))
        missing_arm = copy.deepcopy(row)
        del missing_arm["arms"]["astral"]
        self.assertTrue(rival.evidence_errors(info, [missing_arm]))
        self.assertTrue(rival.evidence_errors(info, []))
        for broken in (dict(samples_s=[]), dict(samples_s=[0.2]), dict(built="cargo: warm"),
                       dict(wall_s=True), dict(wall_s=float("nan"))):
            value = copy.deepcopy(row)
            value["arms"]["astral"]["phases"]["no-op"].update(broken)
            with self.subTest(broken=broken):
                self.assertTrue(rival.evidence_errors(info, [value]))
        legacy = self.root / "legacy"
        (legacy / "raw").mkdir(parents=True)
        del info["schema"]
        info["arms"].append(dict(name="cargo", kind="cargo", commit="old-toolchain"))
        row["arms"]["cargo"] = dict(phases={"cold": dict(wall_s=10, built="cargo: cold")}, wrong=[], failed=None)
        (legacy / "host.json").write_text(json.dumps(info))
        raw = legacy / "raw" / f"{PROJECT}.json"
        raw.write_text(json.dumps(row))
        original = raw.read_bytes()
        with contextlib.redirect_stdout(io.StringIO()):
            self.assertEqual(rival.report(str(legacy)), [])
        self.assertIn("Cargo (historical)", (legacy / "README.md").read_text())
        self.assertIn("10.00!", (legacy / "README.md").read_text())
        self.assertEqual(raw.read_bytes(), original)


if __name__ == "__main__":
    unittest.main()
