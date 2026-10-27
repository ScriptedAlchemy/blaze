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
                "front-end parts: def maps 1.0 MB, impls 2.0 MB, lang items 0.1 MB\n"
                "front-end census: dependency, not frozen: 2 crates, 5 modules, 4 files, 0.3 MB\n"
                "front-end ready\n"
            )
            row = driver.parse(work)
            self.assertEqual(row["crates"], 42)
            self.assertEqual(row["from_image"], 3)
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
        self.assertIsNone(driver.invalid(dict(row, from_image=3, parts=3), "image"))
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
                                   timeout=30, toolchain="1.99.0", package="example")
            with mock.patch.object(driver, "sh", return_value=SimpleNamespace(returncode=0, stdout="")) as sh:
                driver.loadmem(args, "image", env=["RA_JIT_FRONT_IMAGE=/chosen.image"])
                run, stop = sh.call_args_list
                cmd = run.args[0]
                self.assertIn("RA_JIT_FROZEN=on", cmd)
                self.assertLess(cmd.index("RA_JIT_FRONT_IMAGE=off"), cmd.index("RA_JIT_FRONT_IMAGE=/chosen.image"))
                self.assertEqual(stop.args[0], ["/shared/bin/ra-jit", "stop"])
                self.assertEqual(stop.kwargs["env"]["RA_JIT_HOME"], str(Path(work) / "image/home.noindex"))
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


if __name__ == "__main__":
    unittest.main()
