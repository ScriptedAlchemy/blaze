"""Process-level checks for the finite saved-spec cache phase."""
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest


SCRIPT = Path(__file__).with_name("saved_spec.py")


class CachePhaseTests(unittest.TestCase):
    def test_status_and_output_precedence(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            stop = root / "stop"
            stop.write_text("#!" + shutil.which("sh") + "\necho stop-output\nexit \"$STOP_STATUS\"\n")
            stop.chmod(0o755)
            for app_status, stop_status, expected in [(0, 0, 0), (9, 0, 9), (0, 7, 7), (9, 7, 9)]:
                with self.subTest(app=app_status, stop=stop_status):
                    log = root / "stop.log"
                    app = [sys.executable, "-c", f"import sys; print('app-output'); sys.exit({app_status})"]
                    result = subprocess.run([sys.executable, SCRIPT, "--cache-phase", stop, log, *app],
                                            env=dict(os.environ, STOP_STATUS=str(stop_status)),
                                            capture_output=True, text=True, timeout=10)
                    self.assertEqual(result.returncode, expected)
                    self.assertEqual(result.stdout, "app-output\n")
                    self.assertEqual(log.read_text(), "stop-output\n")

    def test_stop_runs_before_a_failed_shutdown_log_write(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            stop, marker = root / "stop", root / "stopped"
            stop.write_text("#!" + shutil.which("sh") + "\nprintf stopped > \"$STOP_MARKER\"\n")
            stop.chmod(0o755)
            for app_status, expected in [(0, 1), (9, 9)]:
                with self.subTest(app=app_status):
                    marker.unlink(missing_ok=True)
                    app = [sys.executable, "-c", f"raise SystemExit({app_status})"]
                    result = subprocess.run(
                        [sys.executable, SCRIPT, "--cache-phase", stop, root / "missing/stop.log", *app],
                        env=dict(os.environ, STOP_MARKER=str(marker)), capture_output=True,
                        text=True, timeout=10,
                    )
                    self.assertEqual(result.returncode, expected, result.stderr)
                    self.assertEqual(marker.read_text(), "stopped")
                    self.assertIn("cannot record cache daemon shutdown", result.stderr)

    @unittest.skipUnless(sys.platform == "linux" and shutil.which("strace"), "strace required")
    def test_trace_waits_for_stopped_descendant(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            stop = root / "stop"
            stop.write_text("#!" + shutil.which("sh") + "\nkill $(cat \"$PID_FILE\")\necho stopped\n")
            stop.chmod(0o755)
            sleeper = shutil.which("sleep")
            self.assertIsNotNone(sleeper)
            app = [sys.executable, "-c", "import os,subprocess,sys; p=subprocess.Popen([sys.argv[1],'60']); open(os.environ['PID_FILE'],'w').write(str(p.pid)); print('42')", sleeper]
            trace, log = root / "trace", root / "stop.log"
            result = subprocess.run(["strace", "-f", "-e", "trace=execve", "-o", trace,
                                     sys.executable, SCRIPT, "--cache-phase", stop, log, *app],
                                    env=dict(os.environ, PID_FILE=str(root / "pid")),
                                    capture_output=True, text=True, timeout=10)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(result.stdout, "42\n")
            self.assertEqual(log.read_text(), "stopped\n")
            self.assertIn('execve("' + sleeper + '"', trace.read_text())


if __name__ == "__main__":
    unittest.main()
