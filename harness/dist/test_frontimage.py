"""Lightweight tests of the measurement driver; no daemon or container starts."""
import importlib.util
import json
from pathlib import Path
import tempfile
import unittest
from unittest import mock
from types import SimpleNamespace


def module(name, path=None):
    spec = importlib.util.spec_from_file_location(name, path or Path(__file__).with_name(name + ".py"))
    loaded = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(loaded)
    return loaded


class FrontImageTests(unittest.TestCase):
    def test_parser_records_real_ready_and_image_counts(self):
        driver = module("frontimage")
        with tempfile.TemporaryDirectory() as work:
            root = Path(work)
            (root / "logs").mkdir()
            (root / "result.json").write_text(json.dumps({
                "outcome": "stopped",
                "stopped_after": {"footprint_mb": 100, "rss_mb": 140},
            }))
            (root / "logs" / "worker.log").write_text(
                "load workspace: 1.2s\nresolve: 42 crates in 30.0ms\n"
                "front-end serve: 10ms in all, 2ms of it reading parts, 3 of 3 parts from the image\n"
                "front-end image usage: 4 attempted, 3 accepted parts\n"
                "front-end parts: def maps 1.0 MB, impls 2.0 MB, lang items 0.1 MB\n"
                "front-end census: dependency, not frozen: 2 crates, 5 modules, 4 files, 0.3 MB\n"
                "front-end ready\n"
            )
            row = driver.parse(work)
            self.assertEqual(row["crates"], 42)
            self.assertEqual(row["from_image"], 3)
            self.assertEqual(row["image_attempted"], 4)
            self.assertEqual(row["image_accepted"], 3)
            self.assertEqual(row["resolve_s"], 0.03)
            self.assertEqual(row["ready_footprint_mb"], 100)
            self.assertEqual(row["census"]["dependency, not frozen"]["crates"], 2)

    def test_readiness_alone_does_not_validate_an_image_or_store_arm(self):
        driver = module("frontimage")
        row = dict(outcome="stopped", tripped=None, ready_logged=True, ready_seen_s=1,
                   load_s=0.5, resolve_s=0.2, ready_footprint_mb=100, ready_rss_mb=140)
        self.assertIn("no mapped parts", driver.invalid(row, "image"))
        self.assertIn("did not read frozen parts", driver.invalid(row, "store"))
        self.assertIn("no frozen parts", driver.invalid(row, "seed"))
        self.assertIsNone(driver.invalid(row, "off"))
        self.assertIsNone(driver.invalid(dict(row, from_image=3, image_accepted=3, parts=3), "image"))
        self.assertIsNotNone(driver.invalid(dict(row, from_image=1, image_attempted=1,
                                                 image_accepted=0, parts=0), "image"))
        # A separately installed store part is not evidence that the unreadable
        # mapped part was installed.
        self.assertIsNotNone(driver.invalid(dict(row, image_attempted=1,
                                                 image_accepted=0, parts=1), "image"))
        self.assertIsNone(driver.invalid(dict(row, from_image=0, parts=3), "store"))
        self.assertIsNone(driver.invalid(dict(row, image_written_parts=3), "seed"))
        self.assertIsNotNone(driver.invalid(dict(row, from_image=3, parts=3), "store"))
        self.assertIsNotNone(driver.invalid(dict(row, parts=3), "off"))

    def test_missing_samples_and_watchdog_failures_are_not_measurements(self):
        driver = module("frontimage")
        row = dict(outcome="stopped", tripped=None, ready_logged=True, ready_seen_s=1,
                   load_s=0.5, resolve_s=0.2, ready_footprint_mb=100, ready_rss_mb=140, from_image=3)
        for key in ("ready_seen_s", "load_s", "resolve_s", "ready_footprint_mb", "ready_rss_mb"):
            with self.subTest(missing=key):
                self.assertIsNotNone(driver.invalid(dict(row, **{key: None}), "image"))
        self.assertIsNotNone(driver.invalid(dict(row, ready_logged=False), "image"))
        self.assertIsNotNone(driver.invalid(dict(row, tripped={"reason": "memory"}), "image"))

    def test_run_isolates_its_configuration_and_stops_only_its_private_daemon(self):
        driver = module("frontimage")
        with tempfile.TemporaryDirectory() as work:
            args = SimpleNamespace(work=work, td=work, blaze_bin="/shared/bin", cargo_home_from=work,
                                   timeout=30, toolchain="1.99.0", package="example", runtime_root=work)
            with mock.patch.object(driver, "sh", return_value=SimpleNamespace(returncode=0, stdout="")) as sh:
                driver.loadmem(args, "image", env=["RA_JIT_FRONT_IMAGE=/chosen.image"])
                run, stop = sh.call_args_list
                cmd = run.args[0]
                self.assertIn("RA_JIT_FROZEN=on", cmd)
                self.assertLess(cmd.index("RA_JIT_FRONT_IMAGE=off"), cmd.index("RA_JIT_FRONT_IMAGE=/chosen.image"))
                self.assertEqual(stop.args[0], ["/shared/bin/ra-jit", "stop"])
                home = stop.kwargs["env"]["RA_JIT_HOME"]
                self.assertIn(home, cmd)
                self.assertFalse(Path(home).exists())
                with self.assertRaises(FileExistsError):
                    driver.loadmem(args, "image")

    def test_watchdog_tracks_only_this_clients_daemon_and_keeps_it_after_reparenting(self):
        watchdog = module("loadmem", Path(__file__).parents[1] / "loadmem.py")
        rows = [(10, 1, 1, "blaze build"), (11, 10, 1, "/shared/ra-jit daemon"),
                (20, 1, 1, "/shared/ra-jit daemon"), (21, 20, 1, "/shared/ra-jit worker")]
        self.assertEqual(watchdog.owned_daemons(rows, 10, set(), "/shared/"), {11})
        rows[1] = (11, 1, 1, "/shared/ra-jit daemon")
        self.assertEqual(watchdog.owned_daemons(rows, 10, {11}, "/shared/"), {11})

    def test_a_ready_marker_cannot_hide_a_watchdog_violation(self):
        watchdog = module("loadmem", Path(__file__).parents[1] / "loadmem.py")
        with tempfile.TemporaryDirectory() as work:
            root = Path(work)
            binary = str(root / "bin")
            proc = SimpleNamespace(pid=10, poll=lambda: None, returncode=None)
            def start(*args, **kwargs):
                home = root / "run/home.noindex"
                home.mkdir()
                (home / "daemon.log").write_text("front-end ready\n")
                return proc
            argv = ["loadmem", "--td", work, "--blaze-bin", binary, "--work", str(root / "run"),
                    "--name", "r", "--watchdog-mb", "1", "--stop-after", "front-end ready", "--stop-delay", "0"]
            rows = [(10, 1, 1, "blaze build"), (11, 10, 10240, binary + "/ra-jit daemon"),
                    (20, 1, 10240, binary + "/ra-jit daemon")]
            with mock.patch.object(watchdog.sys, "argv", argv), \
                 mock.patch.object(watchdog, "DARWIN", False), \
                 mock.patch.object(watchdog.subprocess, "Popen", side_effect=start), \
                 mock.patch.object(watchdog, "ps_table", return_value=rows), \
                 mock.patch.object(watchdog, "swap_mb", return_value=(0, 10000)), \
                 mock.patch.object(watchdog.time, "sleep"), \
                 mock.patch.object(watchdog, "kill") as kill:
                watchdog.main()
            result = json.loads((root / "run/r/result.json").read_text())
            self.assertEqual(result["outcome"], "watchdog-killed")
            self.assertNotIn("stopped_after", result)
            self.assertTrue(all(20 not in call.args[0] for call in kill.call_args_list))

    def test_pressure_rejects_a_container_larger_than_the_guard(self):
        pressure = module("frontimage_pressure")
        import argparse
        with self.assertRaises(ValueError):
            pressure.host(argparse.Namespace(path=__file__, size_mb=6144, extra_mb=100,
                                             runs=3, share=False))

    def test_spread_is_explicit(self):
        driver = module("frontimage")
        self.assertEqual(driver.spread([None]), "-")
        self.assertEqual(driver.spread([1, 2, 3]), "2.00 (1.00-3.00)")

    def test_existing_fallback_is_not_a_successful_measurement(self):
        driver = module("frontimage")
        with tempfile.TemporaryDirectory() as work:
            root = Path(work)
            (root / "logs").mkdir()
            (root / "logs" / "client.log").write_text(
                "ra-jit: bug: this build is cargo's, which ra-jit is never to do\n"
            )
            self.assertTrue(driver.no_rustc_violation(work))
        row = dict(outcome="stopped", tripped=None, ready_logged=True, ready_seen_s=1,
                   load_s=0.5, resolve_s=0.2, ready_footprint_mb=100, ready_rss_mb=140,
                   from_image=3, parts=3, image_written_parts=3, no_rustc_violation=True)
        for arm in ("seed", "off", "store", "image"):
            self.assertIn("fallback", driver.invalid(row, arm))

    def test_long_artifacts_use_short_isolated_runtime_homes(self):
        driver = module("frontimage")
        with tempfile.TemporaryDirectory() as work:
            artifacts = Path(work) / ("long-artifacts-" * 12)
            args = SimpleNamespace(work=str(artifacts), td=work, blaze_bin="/shared/bin",
                                   cargo_home_from=work, timeout=30, toolchain="1.99.0", package="selected",
                                   runtime_root=work)
            homes = []
            with mock.patch.object(driver, "sh", return_value=SimpleNamespace(returncode=0, stdout="")) as sh:
                for arm in ("off", "store", "image"):
                    driver.loadmem(args, arm)
                    home = sh.call_args_list[-1].kwargs["env"]["RA_JIT_HOME"]
                    homes.append(home)
                    self.assertLess(len((home + "/daemon.sock.new/s").encode()), 104)
                    self.assertFalse(Path(home).exists())
                    self.assertTrue((artifacts / arm / "runtime-home.txt").exists())
            self.assertEqual(len(set(homes)), 3)

    def test_timed_arms_never_resolve_unrelated_members_for_census(self):
        driver = module("frontimage")
        # Loaded workspace includes an unrelated package outside selected's closure.
        # Model the census's extra query calls, driven by the actual launch flags.
        workspace = {"selected": ["dependency"], "dependency": [], "unrelated": []}
        with tempfile.TemporaryDirectory() as work:
            args = SimpleNamespace(work=work, td=work, blaze_bin="/shared/bin", cargo_home_from=work,
                                   timeout=30, toolchain="1.99.0", package="selected", runtime_root=work)
            coverage = []
            with mock.patch.object(driver, "sh", return_value=SimpleNamespace(returncode=0, stdout="")) as sh:
                for arm in ("off", "store", "image"):
                    driver.loadmem(args, arm, env=driver.arm_env(arm, "/chosen.image"))
                    cmd = sh.call_args_list[-2].args[0]
                    stats = [cmd[i + 1] for i, v in enumerate(cmd[:-1]) if v == "--env"
                             and cmd[i + 1].startswith("RA_JIT_FRONT_STATS=")][-1]
                    queried = set(workspace) if stats.endswith("=census") else {"selected", "dependency"}
                    self.assertNotIn("unrelated", queried)
                    coverage.append(queried)
            self.assertEqual(coverage[0], coverage[1])
            self.assertEqual(coverage[1], coverage[2])
        self.assertEqual(driver.comparison_kind(["image"]), "diagnostic")
        self.assertEqual(driver.comparison_kind(["off", "store", "image"]), "comparative")

    def test_long_explicit_runtime_home_has_actionable_preflight(self):
        watchdog = module("loadmem", Path(__file__).parents[1] / "loadmem.py")
        with mock.patch.object(watchdog, "DARWIN", True):
            with self.assertRaisesRegex(ValueError, "use --home"):
                watchdog.check_runtime_home("/" + "x" * 104)

    def test_portable_default_ignores_tmpdir(self):
        driver = module("frontimage")
        self.assertEqual(driver.RUNTIME_ROOT, "/tmp")
        with tempfile.TemporaryDirectory() as work:
            args = SimpleNamespace(work=work, td=work, blaze_bin="/shared/bin", cargo_home_from=work,
                                   timeout=30, toolchain="1.99.0", package="selected")
            with mock.patch.dict(driver.os.environ, {"TMPDIR": "/nonexistent/personal/path"}), \
                 mock.patch.object(driver.tempfile, "mkdtemp", return_value=work + "/runtime") as create, \
                 mock.patch.object(driver, "sh", return_value=SimpleNamespace(returncode=0, stdout="")), \
                 mock.patch.object(driver.shutil, "rmtree"):
                driver.loadmem(args, "image")
            self.assertEqual(create.call_args.kwargs["dir"], "/tmp")

    def test_collector_failure_and_stop_failure_preserve_home_and_diagnosis(self):
        driver = module("frontimage")
        for collector_code, stop_code in ((1, 0), (0, 1)):
            with self.subTest(collector=collector_code, stop=stop_code), tempfile.TemporaryDirectory() as work:
                args = SimpleNamespace(work=work, td=work, blaze_bin="/shared/bin", cargo_home_from=work,
                                       timeout=30, toolchain="1.99.0", package="selected", runtime_root=work)
                def run(cmd, **kwargs):
                    if "--home" in cmd:
                        home = Path(cmd[cmd.index("--home") + 1])
                        (home / "daemon.log").write_text("105-byte staging socket diagnosis\n")
                        return SimpleNamespace(returncode=collector_code, stdout="collector output")
                    return SimpleNamespace(returncode=stop_code, stdout="stop output")
                with mock.patch.object(driver, "sh", side_effect=run):
                    with self.assertRaisesRegex(RuntimeError, "runtime home retained"):
                        driver.loadmem(args, "image")
                artifact = Path(work) / "image"
                home = Path((artifact / "runtime-home.txt").read_text().strip())
                self.assertTrue(home.is_dir())
                self.assertIn("105-byte", (home / "daemon.log").read_text())
                self.assertTrue((artifact / "runtime-error.txt").exists())
                if stop_code == 0:
                    self.assertIn("105-byte", (artifact / "r/logs/daemon.log").read_text())

    def test_logs_component_and_stop_time_logs_are_archived_before_removal(self):
        driver = module("frontimage")
        with tempfile.TemporaryDirectory() as work:
            parent = Path(work) / "logs"
            parent.mkdir()
            args = SimpleNamespace(work=work, td=work, blaze_bin="/shared/bin", cargo_home_from=work,
                                   timeout=30, toolchain="1.99.0", package="selected", runtime_root=str(parent))
            def run(cmd, **kwargs):
                if "--home" in cmd:
                    home = Path(cmd[cmd.index("--home") + 1])
                    (home / "daemon.log").write_text("startup\n")
                else:
                    home = Path(kwargs["env"]["RA_JIT_HOME"])
                    with (home / "daemon.log").open("a") as file:
                        file.write("stop-time evidence\n")
                    worker = home / "workers/one"
                    worker.mkdir(parents=True)
                    (worker / "log").write_text("worker evidence\n")
                return SimpleNamespace(returncode=0, stdout="")
            with mock.patch.object(driver, "sh", side_effect=run):
                driver.loadmem(args, "image")
            artifact = Path(work) / "image"
            self.assertIn("stop-time evidence", (artifact / "r/logs/daemon.log").read_text())
            self.assertEqual((artifact / "r/logs/workers_one_log").read_text(), "worker evidence\n")
            self.assertFalse(Path((artifact / "runtime-home.txt").read_text().strip()).exists())

    def test_archive_copy_failure_is_propagated_and_home_retained(self):
        watchdog = module("loadmem", Path(__file__).parents[1] / "loadmem.py")
        driver = module("frontimage")
        with tempfile.TemporaryDirectory() as work:
            home = Path(work) / "logs/runtime"
            home.mkdir(parents=True)
            source = home / "daemon.log"
            source.write_text("original diagnosis")
            client = Path(work) / "artifacts/logs/client.log"
            with mock.patch.object(watchdog.shutil, "copyfile", side_effect=OSError("disk full")):
                with self.assertRaisesRegex(RuntimeError, "could not archive"):
                    watchdog.archive_logs([str(client), str(source)], str(home), work + "/destination", str(client))
            self.assertEqual(source.read_text(), "original diagnosis")
            args = SimpleNamespace(work=work, td=work, blaze_bin="/shared/bin", cargo_home_from=work,
                                   timeout=30, toolchain="1.99.0", package="selected", runtime_root=work)
            with mock.patch.object(driver, "sh", return_value=SimpleNamespace(returncode=0, stdout="")), \
                 mock.patch.object(driver, "archive_runtime", side_effect=RuntimeError("copy failed")):
                with self.assertRaisesRegex(RuntimeError, "copy failed"):
                    driver.loadmem(args, "image")
            retained = Path((Path(work) / "image/runtime-home.txt").read_text().strip())
            self.assertTrue(retained.exists())

    def test_summary_json_validity_and_partial_coverage(self):
        driver = module("frontimage")
        row = dict(outcome="stopped", tripped=None, ready_logged=True, ready_seen_s=1,
                   load_s=0.5, resolve_s=0.2, ready_footprint_mb=100, ready_rss_mb=140,
                   image_accepted=1, image_attempted=2, parts=10, crates=42,
                   workload={"td": "/fixture", "command": ["blaze", "build", "-p", "selected"]})
        summary = {"comparison": "comparative", "arms": {"image": [dict(row, image_accepted=0)]}}
        self.assertTrue(driver.finish_summary(summary))
        self.assertFalse(json.loads(json.dumps(summary))["valid"])
        self.assertTrue(summary["invalid_measurements"])
        summary["arms"]["image"] = [row]
        self.assertFalse(driver.finish_summary(summary))
        self.assertTrue(summary["valid"])
        self.assertEqual(driver.coverage(row), "1/2/10")

    def test_production_main_publishes_failure_and_interrupt_before_any_arm(self):
        driver = module("frontimage")
        for failure in (RuntimeError("collector failed"), KeyboardInterrupt("cancelled")):
            with self.subTest(kind=type(failure).__name__), tempfile.TemporaryDirectory() as work:
                argv = ["frontimage", "--td", work, "--package", "selected", "--blaze-bin", "/shared/bin",
                        "--work", str(Path(work) / "artifacts"), "--cargo-home-from", work]
                def launch(*args, **kwargs):
                    initial = json.loads((Path(work) / "artifacts/summary.json").read_text())
                    self.assertFalse(initial["valid"])
                    self.assertEqual(initial["status"], "incomplete")
                    raise failure
                with mock.patch.object(driver.sys, "argv", argv), mock.patch.object(driver, "loadmem", side_effect=launch):
                    with self.assertRaises(type(failure)):
                        driver.main()
                summary = json.loads((Path(work) / "artifacts/summary.json").read_text())
                self.assertEqual(summary["status"], "interrupted" if isinstance(failure, KeyboardInterrupt) else "failed")
                self.assertFalse(summary["valid"])
                self.assertFalse(summary["comparable"])

    def test_reusing_work_keeps_prior_summary_and_image_untouched(self):
        driver = module("frontimage")
        with tempfile.TemporaryDirectory() as work:
            artifacts = Path(work) / "artifacts"
            (artifacts / "seed").mkdir(parents=True)
            prior = {"summary.json": b'{"status":"complete","valid":true}\n',
                     "front.image": b"the previous run's image"}
            for name, data in prior.items():
                (artifacts / name).write_bytes(data)
            argv = ["frontimage", "--td", work, "--package", "selected", "--blaze-bin", "/shared/bin",
                    "--work", str(artifacts), "--cargo-home-from", work, "--skip-build"]
            with mock.patch.object(driver.sys, "argv", argv):
                with self.assertRaises(FileExistsError):
                    driver.main()
            for name, data in prior.items():
                self.assertEqual((artifacts / name).read_bytes(), data)

    def test_pressure_survival_requires_the_allocator_to_stay_alive(self):
        pressure = module("frontimage_pressure")
        import argparse
        import contextlib
        import io
        for allocated, allocator_exit, expected in ((True, None, True), (False, -9, False),
                                                     (True, -9, False), (True, 0, False)):
            with self.subTest(allocated=allocated, allocator_exit=allocator_exit), \
                 tempfile.TemporaryDirectory() as work:
                image = Path(work) / "front.image"
                image.write_bytes(b"small synthetic image")
                args = argparse.Namespace(path=str(image), size_mb=None, extra_mb=0, runs=1, share=False)
                steps = [{"step": "loaded"}, {"step": "pressed", "b_got_its_memory": allocated,
                                              "b_exit": allocator_exit}]
                output = io.StringIO()
                with mock.patch.object(pressure, "docker", return_value=(0, steps, "")), \
                     contextlib.redirect_stdout(output):
                    self.assertEqual(pressure.host(args), 0)
                rows = [json.loads(line) for line in output.getvalue().splitlines()]
                self.assertEqual(len(rows), 2)
                self.assertTrue(all(row["survived"] is expected for row in rows))

    def test_interrupt_and_failed_stop_still_archive_and_retain_provenance(self):
        driver = module("frontimage")
        with tempfile.TemporaryDirectory() as work:
            args = SimpleNamespace(work=work, td=work, blaze_bin="/shared/bin", cargo_home_from=work,
                                   timeout=30, toolchain="1.99.0", package="selected", runtime_root=work)
            def run(cmd, **kwargs):
                if "--home" in cmd:
                    home = Path(cmd[cmd.index("--home") + 1])
                    (home / "daemon.log").write_text("startup diagnosis")
                    artifact = Path(work) / "image/r"
                    artifact.mkdir()
                    (artifact / "result.json").write_text(json.dumps({"client_pid": 123, "daemon_pids": [456], "exit": -2}))
                    raise KeyboardInterrupt("collector cancelled")
                with (Path(kwargs["env"]["RA_JIT_HOME"]) / "daemon.log").open("a") as log:
                    log.write("\nstop failed diagnosis")
                return SimpleNamespace(returncode=7, stdout="stop failed")
            with mock.patch.object(driver, "sh", side_effect=run):
                with self.assertRaises(KeyboardInterrupt):
                    driver.loadmem(args, "image")
            artifact = Path(work) / "image"
            self.assertIn("stop failed diagnosis", (artifact / "r/logs/daemon.log").read_text())
            cleanup = json.loads((artifact / "cleanup.json").read_text())
            self.assertEqual(cleanup["stop_exit"], 7)
            self.assertEqual(cleanup["source_client_pid"], 123)
            self.assertEqual(cleanup["source_daemon_pids"], [456])
            self.assertTrue(Path(cleanup["runtime_home"]).exists())

    def test_mismatched_workloads_or_resolved_counts_are_not_comparable(self):
        driver = module("frontimage")
        row = dict(outcome="stopped", tripped=None, ready_logged=True, ready_seen_s=1, load_s=1,
                   resolve_s=1, ready_footprint_mb=100, ready_rss_mb=100, crates=2,
                   workload={"td": "/fixture", "command": ["blaze", "build", "-p", "selected"]})
        for other in (dict(row, crates=3), dict(row, workload={"command": ["blaze", "build", "-p", "unrelated"]})):
            summary = {"comparison": "comparative", "arms": {"off": [row], "store": [dict(other, parts=3)],
                                                              "image": [dict(row, parts=3, image_accepted=1)]}}
            self.assertTrue(driver.finish_summary(summary))
            self.assertFalse(summary["comparable"])
            self.assertFalse(summary["valid"])


if __name__ == "__main__":
    unittest.main()
