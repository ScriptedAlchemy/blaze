#!/usr/bin/env python3
"""Controls for source smoke evidence; fixtures below do not compile Rust."""
import contextlib
import importlib.util
import io
import json
import subprocess
from collections.abc import Mapping
from dataclasses import replace
from pathlib import Path
import sys
import tempfile
import unittest
from typing import Any
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "harness/v2"))
import real_smoke as rs

SPEC = importlib.util.spec_from_file_location("source_cli_smoke", ROOT / "harness/blaze_cli.py")
smoke = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = smoke
SPEC.loader.exec_module(smoke)

DOUBLE = r'''#!PYTHON
import json
from pathlib import Path
import sys
binary = Path(__file__).parent
config = json.loads((binary / "control.json").read_text())
command = sys.argv[1]
source = Path("src/main.rs").read_text()
with (binary / "commands.jsonl").open("a") as out:
    out.write(json.dumps(sys.argv[1:]) + "\n")
if command == "check":
    invalid = "missing_source_body" in source
    if config.get("setup_failure"):
        print("error: could not open toolchain shelf", file=sys.stderr)
        sys.exit(1)
    if invalid and not config.get("accept_invalid"):
        print(json.dumps({"severity":"error","code":"types","path":"src/main.rs","line":2,
                          "col":5,"end_line":2,"end_col":24,
                          "message":"cannot find function `missing_source_body` in this scope (segment 0) (E0425 UnresolvedPath)",
                          "fix":None}))
    print(json.dumps({"errors":int(invalid),"warnings":0,"fixes":0,"command":"blaze check",
                      "not_checked":["body"] if config.get("incomplete") else [],
                      "failures":1 if config.get("task_failure") else 0}))
    sys.exit(0 if config.get("accept_invalid") or not invalid else config.get("rejection_exit",1))
value = 7 if "    7\n" in source else 1
if config.get("stale_edit"):
    value = 1
if command == "run":
    print(f"hello from source: {value}")
    if config.get("mutate"):
        with (binary / config["mutate"]).open("ab") as out:
            out.write(b"\n# changed source executable\n")
    sys.exit(config.get("run_exit",0))
if command == "export":
    if not config.get("missing_export"):
        out = Path(sys.argv[-1])
        value = 9 if config.get("wrong_export") else value
        out.write_text("#!/bin/sh\nprintf 'hello from source: " + str(value) + "\\n'\n")
        out.chmod(0o700)
    sys.exit(0)
sys.exit(99)
'''


class SourceSmokeTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.bin = self.root / "bin"
        self.bin.mkdir()
        for name in ("blaze", "blaze-launcher", "blaze-fact-shim"):
            path = self.bin / name
            path.write_text(DOUBLE.replace("#!PYTHON", "#!" + sys.executable))
            path.chmod(0o700)
        (self.bin / "control.json").write_text("{}")
        shelf = self.root / "shelf"
        shelf.mkdir()
        (shelf / "content-addressed-fixture").write_text("shelf input fixture, never loaded by a compiler")
        self.given = smoke.Input(str(self.bin / "blaze"), str(self.root / "run"), str(shelf), 5)

    def exercise(self, **control):
        (self.bin / "control.json").write_text(json.dumps(control))
        with contextlib.redirect_stdout(io.StringIO()):
            smoke.main(self.given)
        return json.loads((self.root / "run/results.json").read_text())

    def rejected_check(self, finding=None, summary=None, **status):
        """The source CLI's observed JSON shape, independent of the matcher implementation."""
        detail = {"id":"q88tn", "severity":"error", "code":"types", "path":"src/main.rs",
                  "line":2, "col":5, "end_line":2, "end_col":24,
                  "message":"cannot find function `missing_source_body` in this scope (segment 0) (E0425 UnresolvedPath)",
                  "fix":None}
        verdict = {"errors":1, "warnings":0, "fixes":0, "command":"blaze check", "not_checked":[]}
        detail.update(finding or {})
        verdict.update(summary or {})
        return {"returncode":1, "timed_out":False, "stderr":"",
                "stdout":json.dumps(detail) + "\n" + json.dumps(verdict) + "\n", **status}

    def test_canonical_types_diagnostic_proves_the_planted_rejection(self):
        record = self.rejected_check()
        self.assertIsNone(smoke.require(record, "invalid", None, rejects=True))
        findings, summary = smoke.check_reply(record)
        self.assertEqual(findings[0]["code"], "types")
        self.assertEqual(summary["errors"], 1)
        self.assertEqual(summary["not_checked"], [])

    def test_arbitrary_types_errors_cannot_replace_the_planted_finding(self):
        prefix = "cannot find function `missing_source_body` in this scope (segment 0) "
        controls = ({"code":"E0425"}, {"severity":"warning"}, {"path":"src/other.rs"},
                    {"line":3}, {"col":6}, {"end_line":3}, {"end_col":23},
                    {"message":None}, {"message":prefix.rstrip()},
                    {"message":prefix + "(E0308 TypeMismatch)"},
                    {"message":prefix + "(E0425 TypeMismatch)"},
                    {"message":prefix + "(E0433 UnresolvedPath)"},
                    {"message":"mismatched type of `missing_source_body` (E0308 TypeMismatch)"},
                    {"message":"unrelated type error mentioning `missing_source_body` (E0425 UnresolvedPath)"},
                    {"message":"cannot find function `another_body` in this scope (segment 0) (E0425 UnresolvedPath)"})
        for finding in controls:
            with self.subTest(finding=finding), self.assertRaises(smoke.Failed):
                smoke.require(self.rejected_check(finding=finding), "invalid", None, rejects=True)

    def test_the_rejection_requires_a_complete_failed_check_verdict(self):
        for summary in ({"errors":0}, {"errors":2}, {"errors":True}, {"failures":1},
                        {"not_checked":["body"]}, {"not_checked":None}, {"command":"blaze run"}):
            with self.subTest(summary=summary), self.assertRaises(smoke.Failed):
                smoke.require(self.rejected_check(summary=summary), "invalid", None, rejects=True)
        for status in ({"returncode":0}, {"returncode":3}, {"timed_out":True}):
            with self.subTest(status=status), self.assertRaises(smoke.Failed):
                smoke.require(self.rejected_check(**status), "invalid", None, rejects=True)

    def test_every_source_phase_runs_and_exported_program_is_executed(self):
        result = self.exercise()
        self.assertIs(result["complete"], True)
        self.assertEqual([row["name"] for row in result["phases"]], list(smoke.PHASES))
        self.assertEqual(set(result["binaries"]), {"blaze", "blaze-launcher", "blaze-fact-shim"})
        self.assertEqual(len(result["artifact_sha256"]), 64)
        commands = [json.loads(line) for line in (self.bin / "commands.jsonl").read_text().splitlines()]
        self.assertEqual([row[0] for row in commands], ["run", "run", "run", "check", "run", "check", "export"])
        self.assertEqual([row for row in commands if row[0] == "check"], [["check", "--view", "json"]] * 2)
        self.assertEqual((self.root / "run/standalone.json").exists(), True)
        self.assertNotEqual(result["phases"][0]["source_sha256"], result["phases"][2]["source_sha256"])

    def test_prior_evidence_is_never_erased(self):
        self.exercise()
        original = (self.root / "run/results.json").read_bytes()
        with self.assertRaisesRegex(smoke.Failed, "new or empty"):
            self.exercise()
        self.assertEqual((self.root / "run/results.json").read_bytes(), original)

    def test_wrong_output_and_successful_setup_failures_cannot_count(self):
        controls = ({"stale_edit":True}, {"run_exit":23}, {"setup_failure":True},
                    {"task_failure":True}, {"accept_invalid":True}, {"rejection_exit":3},
                    {"incomplete":True}, {"missing_export":True}, {"wrong_export":True})
        for control in controls:
            with self.subTest(control=control):
                # Every plant has independent sources/evidence; no baseline from a previous run.
                self.given = smoke.Input(self.given.blaze, str(self.root / ("run-" + next(iter(control)))), self.given.shelf, 5)
                with self.assertRaises(smoke.Failed):
                    self.exercise(**control)
                result = json.loads((Path(self.given.scratch) / "results.json").read_text())
                self.assertIs(result["complete"], False)
                self.assertTrue(result["error"])

    def test_a_changed_helper_cannot_earn_acceptance(self):
        for name in ("blaze", "blaze-launcher", "blaze-fact-shim"):
            with self.subTest(binary=name):
                self.given = smoke.Input(self.given.blaze, str(self.root / ("mutate-" + name)), self.given.shelf, 5)
                with self.assertRaisesRegex(smoke.Failed, "changed"):
                    self.exercise(mutate=name)
                result = json.loads((Path(self.given.scratch) / "results.json").read_text())
                self.assertIs(result["complete"], False)

    def test_missing_helper_and_symlink_are_refused_before_running(self):
        helper = self.bin / "blaze-launcher"
        helper.unlink()
        with self.assertRaisesRegex(smoke.Failed, "regular executable"):
            self.exercise()
        helper.symlink_to(self.bin / "blaze")
        with self.assertRaisesRegex(smoke.Failed, "regular executable"):
            self.exercise()
        self.assertFalse((self.bin / "commands.jsonl").exists())

    def test_input_is_typed_and_has_no_phase_filter(self):
        good = {"blaze":"/bin/blaze", "scratch":"/new", "shelf":"/shelf"}
        self.assertEqual(smoke.load(json.dumps(good)).timeout, 1800)
        for changed in ({"timeout":True}, {"timeout":0}, {"only":["warm"]}, {"blaze":""}):
            with self.subTest(changed=changed), self.assertRaises((smoke.Failed, TypeError)):
                smoke.load(json.dumps({**good, **changed}))


class RealSmokeTests(unittest.TestCase):
    def setUp(self) -> None:
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name).resolve()
        self.bin = self.root / "bin"
        self.bin.mkdir()
        for name in ("blaze", "blaze-launcher", "blaze-fact-shim"):
            path = self.bin / name
            path.write_text(f"synthetic {name}; never executed\n")
            path.chmod(0o700)
        self.builder = self.root / "proc_macro_modules"
        self.builder.write_text("synthetic module builder; never executed\n")
        self.builder.chmod(0o700)
        self.shelf = self.root / "shelf"
        self.shelf.mkdir()
        (self.shelf / "manifest-record").write_text("synthetic shelf\n")
        self.toolchain_home = self.root / "toolchain-home"
        (self.toolchain_home / ".rustup" / "toolchains").mkdir(parents=True)
        self.given = rs.Input(
            str(self.bin),
            str(self.root / "work"),
            "",
            modules=str(self.builder),
            toolchain_home=str(self.toolchain_home),
            shelf=str(self.shelf),
        )

    def invoke(self, given: rs.Input | None = None, code: int = 1) -> None:
        given = given or self.given
        with contextlib.redirect_stdout(io.StringIO()), self.assertRaises(SystemExit) as ended:
            rs.main(
                [
                    "real_smoke.py",
                    json.dumps(
                        {
                            "blaze": given.blaze,
                            "work": given.work,
                            "tmpfs": given.tmpfs,
                            "modules": given.modules,
                            "toolchain_home": given.toolchain_home,
                            "shelf": given.shelf,
                        }
                    ),
                ]
            )
        self.assertEqual(ended.exception.code, code)

    def fake_builder(self, made: dict[str, int]) -> tuple[list[dict[str, Any]], list[dict[str, str]], Any]:
        """A module builder that writes `made[kind]` module records under the store it is given
        and records each request and environment it was handed."""
        requests: list[dict[str, Any]] = []
        environments: list[dict[str, str]] = []

        def run(argv: list[str], **kwargs: Any) -> subprocess.CompletedProcess[str]:
            if Path(argv[0]) != self.builder:
                # `blaze daemon --stop`: never started; the harness only reads its exit status.
                return subprocess.CompletedProcess(argv, 0, "", "")
            request = json.loads(argv[1])
            requests.append(request)
            environments.append(kwargs["env"])
            inner = request.get("build_scripts", request)
            kind = "build-script" if "build_scripts" in request else "proc-macro"
            for at in range(made.get(kind, 0)):
                record = Path(inner["store"]) / kind / "modules" / f"{at:064x}"
                record.mkdir(parents=True)
                (record / "module.wasm").write_text(f"synthetic {kind} module {at}\n")
                (record / "identity").write_text(f"name synthetic{at}\n")
            return subprocess.CompletedProcess(argv, 0, "", "")

        return requests, environments, patch.object(rs.subprocess, "run", side_effect=run)

    def test_modules_are_asked_of_the_lock_with_the_sandbox_and_copied_into_the_home(self) -> None:
        requests, environments, builder = self.fake_builder(
            {"proc-macro": len(rs.MACROS), "build-script": len(rs.SCRIPTS)}
        )
        rows: list[tuple[str, Mapping[str, str]]] = []

        def ran(command: rs.Command, proof: rs.Proof) -> rs.ran.Ran:
            rows.append((command.args[3], command.env))
            return rs.ran.Ran(1.0, 0, proof.stdout, "")

        with builder, patch.object(rs.ran, "run", side_effect=ran):
            self.invoke(code=0)
        self.assertEqual([("build_scripts" in it) for it in requests], [False, True])
        for request in requests:
            inner = request.get("build_scripts", request)
            self.assertEqual(Path(inner["checkout"]), self.root / "work" / "p")
            self.assertEqual(Path(inner["tree"]), rs.TREE)
            self.assertIs(inner["unsandboxed_build_scripts"], False)
            self.assertNotIn("vendor_from", inner)
        self.assertEqual(requests[0]["crates"], list(rs.MACROS))
        self.assertEqual(requests[1]["build_scripts"]["crates"], list(rs.SCRIPTS))
        for environment in environments:
            self.assertEqual(
                environment,
                {"HOME": str(self.toolchain_home), "PATH": self.given.path, "RUST_BACKTRACE": "0"},
            )
        home = self.root / "work" / "home" / ".blaze" / "v2"
        self.assertEqual(len(list((home / "proc-macro" / "modules").glob("*/module.wasm"))), len(rs.MACROS))
        self.assertEqual(
            len(list((home / "build-script" / "modules").glob("*/module.wasm"))), len(rs.SCRIPTS)
        )
        self.assertTrue((home / "toolchain" / "manifest-record").is_file())
        self.assertEqual([name for name, _ in rows], ["smoke", "smoke", "smoke", "smoke-edition"])
        for _, env in rows:
            self.assertEqual(env["HOME"], str(self.root / "work" / "home"))
            self.assertEqual(env["RUST_BACKTRACE"], "0")
            self.assertNotIn("CARGO_TARGET_DIR", env)
        for step in ("cold", "noop", "edit", "edition"):
            self.assertTrue((self.root / "work" / "logs" / f"{step}.stdout").is_file())

    def test_a_failed_or_short_module_build_is_invalid_and_nothing_is_copied(self) -> None:
        for made, code in (({"proc-macro": len(rs.MACROS), "build-script": len(rs.SCRIPTS) - 1}, 1),):
            with self.subTest(made=made):
                _, _, builder = self.fake_builder(made)
                out = io.StringIO()
                with builder, contextlib.redirect_stdout(out), self.assertRaises(SystemExit) as ended:
                    rs.main(
                        [
                            "real_smoke.py",
                            json.dumps(
                                {
                                    "blaze": str(self.bin),
                                    "work": str(self.root / f"w{code}"),
                                    "tmpfs": "",
                                    "modules": str(self.builder),
                                    "toolchain_home": str(self.toolchain_home),
                                    "shelf": str(self.shelf),
                                }
                            ),
                        ]
                    )
                self.assertEqual(ended.exception.code, 1)
                self.assertIn("INVALID", out.getvalue())
                self.assertFalse(
                    (self.root / f"w{code}" / "home" / ".blaze" / "v2" / "build-script").exists()
                )

    def test_without_a_builder_no_module_is_asked_for(self) -> None:
        requests, _, builder = self.fake_builder({})
        with (
            builder,
            patch.object(
                rs.ran, "run", return_value=rs.ran.Ran(1.0, 125, "", "refused", "exit 125, not 0")
            ) as run,
        ):
            self.invoke(replace(self.given, modules=""), code=1)
        self.assertEqual(requests, [])
        self.assertEqual(run.call_count, 4)

    def test_the_edition_member_names_nothing_of_its_standard_library(self) -> None:
        manifest = (rs.SOURCE / "edition" / "Cargo.toml").read_text()
        self.assertIn('edition = "blaze-2026.11"', manifest)
        self.assertNotIn("bstd", manifest.split("[package]")[1])
        self.assertNotIn("blaze-std", manifest.split("[package]")[1])
        main = (rs.SOURCE / "edition" / "src" / "main.rs").read_text()
        self.assertIn("fn main(cx: &Cx, out: Out) -> Result<(), Error>", main)
        self.assertNotIn("io: Io", main)
        self.assertIn(rs.EDITION, main)

    def test_the_lock_holds_the_workspace_roots_and_no_bstd_copy(self) -> None:
        lock = (rs.SOURCE / "Cargo.lock").read_text()
        self.assertIn('name = "smoke-edition"\nversion = "0.1.0"\n\n', lock)
        self.assertNotIn("blaze-std", lock)
        for crate in (*rs.MACROS, *rs.SCRIPTS):
            self.assertIn(f'name = "{crate["name"]}"\nversion = "{crate["version"]}"\n', lock)

    def test_the_input_is_refused(self) -> None:
        base = {"blaze": str(self.bin), "work": str(self.root / "w"), "tmpfs": ""}
        for name, value, said in (
            ("bogus", 1, "not a field of Input"),
            ("workers", 0, "positive"),
            ("workers", "4", "is a int"),
            ("steps", ["edit"], "needs `cold`"),
            ("steps", ["noop", "cold"], "in that order"),
            ("modules", str(self.root / "absent"), "not an executable"),
            ("toolchain_home", str(self.root / "absent"), "holds no .rustup/toolchains"),
        ):
            with self.subTest(name=name, value=value), self.assertRaisesRegex(SystemExit, said):
                rs.main(["real_smoke.py", json.dumps({**base, name: value})])
        with self.assertRaisesRegex(SystemExit, "toolchain_home is needed with modules"):
            rs.main(["real_smoke.py", json.dumps({**base, "modules": str(self.builder)})])
        for name in ("blaze", "blaze-launcher", "blaze-fact-shim"):
            with self.subTest(name=name):
                path = self.bin / name
                path.unlink()
                with self.assertRaisesRegex(SystemExit, f"holds no `{name}`"):
                    rs.main(["real_smoke.py", json.dumps(base)])
                path.write_text("synthetic\n")
                path.chmod(0o700)


if __name__ == "__main__":
    unittest.main()
