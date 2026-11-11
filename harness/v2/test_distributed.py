"""D11 adapter controls. Synthetic executables are never run; no Blaze process is started.

The success controls test adapter wiring only. Failure controls also enter the real
distv2.main, with its external services/build outcome controlled, and require its genuine
acceptance and finally-cleanup failures to make the ledger result fail.
"""

import contextlib
import io
import json
import os
import shutil
import subprocess
import tempfile
import unittest
from dataclasses import replace
from pathlib import Path
from unittest.mock import patch

import distributed as d
import ledger


class DistributedLedgerTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name).resolve()
        self.bin = self.root / "bin"
        self.bin.mkdir()
        self.source = {"commit": "11" * 20, "tree": "22" * 20}
        source = patch.object(d.identity, "source", side_effect=lambda: dict(self.source))
        source.start()
        self.addCleanup(source.stop)
        for name in ("blaze", "blaze-engine", "blaze-dist-local", "blaze-serve"):
            path = self.bin / name
            path.write_text(f"synthetic {name}; never executed\n")
            path.chmod(0o700)
        self.seal()
        self.reflink = patch.object(d.clone_out, "selftest", return_value=None)
        self.reflink.start()
        self.addCleanup(self.reflink.stop)
        for patcher in (patch.object(d.report, "host", return_value={}),
                        patch.object(d.report, "commit", return_value=self.source["commit"])):
            patcher.start()
            self.addCleanup(patcher.stop)
        self.given = d.Input(str(self.bin / "blaze"), str(self.root / "result.json"),
                             cores=2, daemon_memory_gb=6)

    def seal(self, services=True):
        record = d.identity.identity(self.bin, services)
        (self.bin / d.identity.MANIFEST).write_text(json.dumps(record))
        return record

    def invoke(self, given=None, code=1):
        given = given or self.given
        with contextlib.redirect_stdout(io.StringIO()), self.assertRaises(SystemExit) as ended:
            d.main(given)
        self.assertEqual(ended.exception.code, code)
        return json.loads(Path(given.out).read_text())

    def test_complete_profiles_reuse_the_sealed_programs_and_separate_fresh_scratch(self):
        seen = []

        def complete(given):
            scratch = Path(given.scratch)
            self.assertFalse(scratch.exists())
            scratch.mkdir()
            (scratch / "synthetic-wiring-only").write_text("not distributed acceptance\n")
            seen.append(given)

        with patch.object(d.distv2, "main", side_effect=complete), \
                patch.object(d, "verified_build", wraps=d.verified_build) as verified:
            result = self.invoke(code=0)
        self.assertEqual(verified.call_count, 5)  # Initial seal, then before/after each main.
        self.assertEqual([it.profile for it in seen], ["debug", "release"])
        self.assertEqual([it.args for it in seen], [(), ("--release",)])
        self.assertEqual([it.release_fold for it in seen], [False, True])
        evidence = Path(self.given.out + ".distv2")
        self.assertEqual([Path(it.scratch) for it in seen], [evidence / "dev", evidence / "release"])
        for it in seen:
            self.assertTrue(it.faults)
            self.assertEqual((it.cores, it.daemon_memory_gb), (2, 6))
            self.assertEqual(it.blaze, str(self.bin / "blaze"))
            self.assertEqual(it.dist_local, str(self.bin / "blaze-dist-local"))
            self.assertEqual(it.serve, str(self.bin / "blaze-serve"))
            self.assertEqual(it.env, {"BLAZE_LOCK": "write", "XDG_CACHE_HOME": str(Path(it.scratch) / "machine-cache")})
        self.assertEqual([(it["name"], it["verdict"]) for it in result["outcomes"]],
                         [("distv2-dev", "pass"), ("distv2-release", "pass")])
        self.assertEqual(set(result["extra"]["build_identity"]["binaries"]),
                         {"blaze", "blaze-engine", "blaze-dist-local", "blaze-serve"})
        self.assertEqual(json.loads((evidence / "build-identity.json").read_text()),
                         result["extra"]["build_identity"])

    def test_input_cannot_filter_profiles_disable_faults_or_replace_sealed_services(self):
        base = {"blaze": self.given.blaze, "out": self.given.out, "cores": 2, "daemon_memory_gb": 6}
        for name, value in (("profile", "debug"), ("faults", False), ("release_fold", False),
                            ("dist_local", "another-program"), ("serve", "another-program")):
            with self.subTest(name=name), self.assertRaisesRegex(SystemExit, "no field"):
                d.parse(d.Input, json.dumps({**base, name: value}), d.SCRIPT)
        with self.assertRaisesRegex(SystemExit, "cores"):
            d.parse(d.Input, json.dumps({**base, "cores": True}), d.SCRIPT)

    def test_resources_are_required_and_cannot_take_ambient_values(self):
        base = {"blaze": self.given.blaze, "out": self.given.out, "cores": 2, "daemon_memory_gb": 6}
        with patch.dict(os.environ, {"BLAZE_CORES": "99", "BLAZE_DAEMON_MEMORY_GB": "99"}):
            for name in ("cores", "daemon_memory_gb"):
                with self.subTest(name=name), self.assertRaisesRegex(SystemExit, f"field {name} is required"):
                    d.parse(d.Input, json.dumps({key: value for key, value in base.items() if key != name}), d.SCRIPT)
                with self.subTest(name=name), self.assertRaisesRegex(SystemExit, name):
                    d.parse(d.Input, json.dumps({**base, name: None}), d.SCRIPT)
                for value in (None, 0, -1, True):
                    with self.subTest(name=name, value=value), self.assertRaises((ValueError, d.distv2.Failed)):
                        d.profile_input(replace(self.given, **{name: value}), self.bin / "blaze", self.root, "dev")
            profile = d.profile_input(self.given, self.bin / "blaze", self.root, "dev")
            run = object.__new__(d.distv2.Run)  # Exercise only its real child-environment construction.
            run.given, run.root = profile, Path(profile.scratch)
            run.root.mkdir()
            run.socket, run.port = self.root / "unused.sock", 1
            child = run.env("a", True)
        self.assertEqual((child["BLAZE_CORES"], child["BLAZE_DAEMON_MEMORY_GB"]), ("2", "6"))

    def test_each_sealed_program_and_the_services_manifest_are_required_before_acceptance(self):
        for name in ("blaze", "blaze-engine", "blaze-dist-local", "blaze-serve"):
            with self.subTest(name=name):
                path = self.bin / name
                original = path.read_bytes()
                path.unlink()
                with patch.object(d.distv2, "main") as acceptance:
                    result = self.invoke(replace(self.given, out=str(self.root / f"missing-{name}.json")))
                acceptance.assert_not_called()
                self.assertEqual(result["counts"], {"error": 1})
                path.write_bytes(original)
                path.chmod(0o700)
        self.seal(services=False)
        with patch.object(d.distv2, "main") as acceptance:
            result = self.invoke()
        acceptance.assert_not_called()
        self.assertIn("executable set", result["outcomes"][0]["detail"])

    def test_a_different_source_or_a_dirty_source_cannot_start_acceptance(self):
        self.source["commit"] = "33" * 20
        with patch.object(d.distv2, "main") as acceptance:
            result = self.invoke()
        acceptance.assert_not_called()
        self.assertIn("build source", result["outcomes"][0]["detail"])
        with patch.object(d.identity, "source", side_effect=d.identity.Refused("source checkout has changes")), \
                patch.object(d.distv2, "main") as acceptance:
            result = self.invoke(replace(self.given, out=str(self.root / "dirty.json")))
        acceptance.assert_not_called()
        self.assertIn("source checkout has changes", result["outcomes"][0]["detail"])

    def test_resealing_changed_engine_bytes_between_profiles_cannot_change_this_runs_identity(self):
        def replace_engine(_given):
            (self.bin / "blaze-engine").write_text("different engine, still never executed\n")
            self.seal()  # The current manifest is internally valid, but is a different build.

        with patch.object(d.distv2, "main", side_effect=replace_engine) as acceptance:
            result = self.invoke()
        self.assertEqual(acceptance.call_count, 1)
        self.assertEqual(result["counts"], {"fail": 1, "error": 1})
        self.assertIn("identity after dev cleanup", result["outcomes"][0]["detail"])
        self.assertIn("build binaries differ", result["outcomes"][0]["detail"])

    def test_reused_evidence_or_report_is_refused_and_the_earlier_bytes_remain(self):
        evidence = Path(self.given.out + ".distv2")
        evidence.mkdir()
        retained = evidence / "earlier-evidence"
        retained.write_bytes(b"keep this evidence")
        with patch.object(d.distv2, "main") as acceptance:
            result = self.invoke()
        acceptance.assert_not_called()
        self.assertIn("already exists", result["outcomes"][0]["detail"])
        self.assertEqual(retained.read_bytes(), b"keep this evidence")
        earlier_report = Path(self.given.out).read_bytes()
        with contextlib.redirect_stdout(io.StringIO()), self.assertRaisesRegex(SystemExit, "report .* already exists"):
            d.main(self.given)
        self.assertEqual(Path(self.given.out).read_bytes(), earlier_report)

    def test_a_copy_instead_of_shared_clone_blocks_is_refused_before_acceptance(self):
        self.reflink.stop()
        # Enter the real clone oracle. A copying backend keeps every byte private and
        # must not become a supported-volume fallback merely because the bytes match.
        with patch.object(d.clone_out, "clone", side_effect=shutil.copyfile), \
                patch.object(d.clone_out, "private_bytes", side_effect=lambda path: path.stat().st_size), \
                patch.object(d.distv2, "main") as acceptance:
            result = self.invoke()
        acceptance.assert_not_called()
        self.assertIn("did not prove reflink placement", result["outcomes"][0]["detail"])
        self.assertIn("a clone shares every block", result["outcomes"][0]["detail"])

    def test_an_exit_or_nonzero_return_from_acceptance_is_never_a_pass(self):
        for ordinal, exit_or_return in enumerate((SystemExit(23), 23)):
            with self.subTest(exit_or_return=exit_or_return):
                supplied = replace(self.given, out=str(self.root / f"exit-{ordinal}.json"))
                arguments = ({"side_effect": exit_or_return} if isinstance(exit_or_return, SystemExit)
                             else {"return_value": exit_or_return})
                with patch.object(d.distv2, "main", **arguments) as acceptance:
                    result = self.invoke(supplied)
                self.assertEqual(acceptance.call_count, 1)
                self.assertEqual(result["counts"], {"fail": 1, "error": 1})
                self.assertIn("23", result["outcomes"][0]["detail"])

    def test_real_distv2_main_propagates_a_failed_build_and_runs_its_cleanup(self):
        stopped = []
        original_stop = d.distv2.Run.stop

        def stop(run):
            original_stop(run)
            stopped.append(run.root)

        with patch.object(d.distv2, "free_port", return_value=1), \
                patch.object(d.distv2, "source_state", return_value=(self.source["commit"], self.source["tree"], b"")), \
                patch.object(d.distv2.Run, "start_services", return_value=None), \
                patch.object(d.distv2.Run, "build", return_value=(23, "controlled build failure")) as build, \
                patch.object(d.distv2.Run, "stop", new=stop):
            result = self.invoke()
        self.assertEqual(build.call_count, 1)
        self.assertEqual(stopped, [Path(self.given.out + ".distv2") / "dev"])
        self.assertEqual(result["counts"], {"fail": 1, "error": 1})
        self.assertIn("solo builds: controlled build failure", result["outcomes"][0]["detail"])
        log = Path(result["outcomes"][0]["numbers"]["log"]).read_text()
        self.assertIn("FAIL solo builds: controlled build failure", log)

    def test_real_distv2_finally_failure_and_post_cleanup_identity_failure_are_both_reported(self):
        original_stop = d.distv2.Run.stop

        def stop(run):
            original_stop(run)
            (self.bin / "blaze-engine").write_text("changed during cleanup; never executed\n")
            raise d.distv2.Failed("controlled owned-process cleanup failure")

        with patch.object(d.distv2, "free_port", return_value=1), \
                patch.object(d.distv2, "source_state", return_value=(self.source["commit"], self.source["tree"], b"")), \
                patch.object(d.distv2.Run, "start_services", return_value=None), \
                patch.object(d.distv2.Run, "build", return_value=(23, "controlled build failure")), \
                patch.object(d.distv2.Run, "stop", new=stop):
            result = self.invoke()
        self.assertEqual(result["counts"], {"fail": 1, "error": 1})
        detail = result["outcomes"][0]["detail"]
        self.assertIn("controlled owned-process cleanup failure", detail)
        self.assertIn("identity after dev cleanup", detail)
        self.assertIn("build binaries differ", detail)

    def test_the_registered_check_runs_through_the_real_ledger_input_adapter(self):
        rows = d.parse(ledger.Ledger, str(d.ROOT / ledger.DATA), ledger.SCRIPT)
        work = self.root / "ledger-work"
        passed = []

        def child(argv, **_kwargs):
            if argv[0] == "git":
                answer = self.source["commit"] if argv[-2:] == ["rev-parse", "HEAD"] else ""
                return subprocess.CompletedProcess(argv, 0, stdout=answer)
            self.assertEqual(argv[:4], ["uv", "run", "--frozen", "python"])
            self.assertEqual(argv[4], "harness/v2/distributed.py")
            given = d.parse(d.Input, argv[5], d.SCRIPT)
            passed.append(given)
            try:
                d.main(given)
            except SystemExit as ended:
                return subprocess.CompletedProcess(argv, ended.code if type(ended.code) is int else 1)
            self.fail("the adapter must finish with its ledger report's exit status")

        with contextlib.redirect_stdout(io.StringIO()), \
                patch.object(d.distv2, "main", return_value=None) as acceptance, \
                patch.object(ledger.subprocess, "run", side_effect=child):
            updated = ledger.run(ledger.Input("run", engine=d.EngineSpec("v2", self.given.blaze),
                                              work=str(work), ids=("D11",)), d.ROOT, rows)
        self.assertEqual(len(passed), 1)
        self.assertEqual(passed[0].blaze, self.given.blaze)
        self.assertEqual(passed[0].out, str(work / "D11-0.json"))
        self.assertEqual((passed[0].cores, passed[0].daemon_memory_gb), (2, 6))
        self.assertEqual(acceptance.call_count, 2)
        result = json.loads((work / "D11-0.json").read_text())
        self.assertEqual(result["counts"], {"pass": 2})
        row = next(it for it in updated.rows if it.id == "D11")
        self.assertEqual(row.status.state, "open" if row.missing else "met")
        if row.missing:
            self.assertIn("its checks pass; still to write:", row.status.detail)


if __name__ == "__main__":
    unittest.main()
