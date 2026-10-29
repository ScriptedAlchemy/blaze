#!/usr/bin/env python3
"""Process-sampler failures and environment-free watchdog sampling; no builds or allocators.

Run as `python3 -I harness/tracedecay_test.py`.
"""
import contextlib, io, os, platform, subprocess, sys, unittest
from unittest import mock

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import tracedecay as t


class ProcessTable(unittest.TestCase):
    def setUp(self):
        for patch in (mock.patch.object(t.platform, "system", return_value="Linux"),
                      mock.patch.object(t.os, "getpid", return_value=7),
                      mock.patch.object(t.os, "readlink", return_value="7")):
            patch.start()
            self.addCleanup(patch.stop)

    def test_mismatched_procfs_refuses_even_when_host_pid_collides(self):
        # /proc/7 may exist, but it is not the process whose local PID is 7.
        with mock.patch.object(t.os, "readlink", return_value="1007"), \
                mock.patch.object(t, "sh", return_value=subprocess.CompletedProcess(
                    [], 0, "7 2 1024 unrelated-host-process\n")) as command:
            with self.assertRaisesRegex(t.ProcessSamplingError, "different PID namespace"):
                t.ps_table(env=False)
            command.assert_not_called()

    def test_unreadable_procfs_is_explicit(self):
        with mock.patch.object(t.os, "readlink", side_effect=OSError), \
                self.assertRaisesRegex(t.ProcessSamplingError, "cannot identify Linux /proc/self"):
            t.ps_table(env=False)

    def test_ps_failure_is_not_an_empty_or_partial_measurement(self):
        for stdout in ("", "7 2 1024 private-environment\n"):
            with self.subTest(stdout=bool(stdout)), mock.patch.object(t, "sh", return_value=
                    subprocess.CompletedProcess([], 1, stdout, "private-diagnostic")):
                with self.assertRaisesRegex(t.ProcessSamplingError, "ps exited with status 1") as error:
                    t.ps_table()
                self.assertNotIn("private", str(error.exception))

    def test_missing_current_process_is_not_a_valid_table(self):
        for stdout in ("", "malformed output\n", "8 2 1024 other-process\n"):
            with self.subTest(stdout=stdout), mock.patch.object(t, "sh", return_value=
                    subprocess.CompletedProcess([], 0, stdout)):
                with self.assertRaisesRegex(t.ProcessSamplingError, "omitted the current process"):
                    t.ps_table(env=False)

    def test_wide_environment_mode_preserves_native_bytes_on_both_platforms(self):
        command_line = "python RA_JIT_RUN_ID=run-\udcff"
        for system, flag in (("Linux", "eww"), ("Darwin", "-Eww")):
            with self.subTest(system=system), mock.patch.object(t.platform, "system", return_value=system), \
                    mock.patch.object(t, "sh", return_value=subprocess.CompletedProcess(
                        [], 0, f"7 2 1024 {command_line}\n")) as command:
                self.assertEqual(t.ps_table(), [(7, 2, 1024, command_line)])
                command.assert_called_once_with(["ps", flag, "-Ao", "pid=,ppid=,rss=,command="],
                                                errors="surrogateescape", stderr=subprocess.PIPE)


class EnvironmentFree(unittest.TestCase):
    def test_rss_measurement_does_not_read_environments(self):
        with mock.patch.object(t.shutil, "which", return_value=None), \
                mock.patch.object(t, "ps_table", return_value=[(7, 2, 2048, "python")]) as table:
            self.assertEqual(t.footprints_mb([7]), {7: 2.0})
            table.assert_called_once_with(env=False)

    def test_watchdog_opt_out_applies_to_sampling_and_kill_cleanup(self):
        watch = t.Watchdog("/unused", process_env=False, limit_mb=1,
                           swap_fn=lambda: (0, 65536))
        watch.client = lambda: 7
        watch.pids = lambda rows, client_tree: client_tree
        with mock.patch.object(t, "ps_table", return_value=[(7, 2, 2048, "python")]) as table, \
                mock.patch.object(t, "footprints_mb", return_value={7: 2.0}), \
                mock.patch.object(t, "kill_pids"), mock.patch.object(t.time, "sleep"), \
                contextlib.redirect_stdout(io.StringIO()):
            self.assertIsNotNone(watch.sample())
            self.assertEqual(table.call_args_list, [mock.call(env=False), mock.call(env=False)])
        self.assertEqual(watch.samples, 1)
        self.assertEqual(watch.peak_mb, 2.0)

    @unittest.skipUnless(platform.system() == "Linux", "Linux procfs smoke test")
    def test_real_linux_sampler_measures_or_names_namespace_mismatch(self):
        if int(os.readlink("/proc/self")) != os.getpid():
            with self.assertRaisesRegex(t.ProcessSamplingError, "different PID namespace"):
                t.ps_table(env=False)
        else:
            rows = t.ps_table(env=False)
            self.assertTrue(any(pid == os.getpid() and rss > 0 for pid, _, rss, _ in rows))


if __name__ == "__main__":
    unittest.main()
