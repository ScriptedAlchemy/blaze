#!/usr/bin/env python3
"""Exercise real subprocess capture; the exec tracer and watchdog verdict are injected.

These tests check failure evidence, not compiler behavior or watchdog sampling.
"""
import importlib.util
import json
import os
from pathlib import Path
import signal
import sys
import tempfile
import threading
import types
import unittest
from unittest.mock import patch


class Watchdog:
    verdict = {}
    made = []

    def __init__(self, root, memory, swap, period, **kwargs):
        self.stop = threading.Event()
        self.limits = (memory, swap)
        self.made.append(self)

    def start(self):
        pass

    def join(self):
        pass

    def take(self):
        return dict(self.verdict)


def kill_children(pids):
    for pid in pids:
        try:
            os.killpg(pid, signal.SIGKILL)
        except ProcessLookupError:
            pass


def load(path):
    # Sampling and tracing are tested by their own suites. Here a real child writes
    # real streams and the guard receives an injected decision, including rejection.
    tracing = types.SimpleNamespace(exec_trace=lambda argv, trace, **kw: argv)
    watching = types.SimpleNamespace(
        Watchdog=Watchdog, SWAP_FREE_MIN_MB=3072,
        ps_table=lambda **kw: [], descendants=lambda rows, roots: set(),
        kill_pids=kill_children,
    )
    spec = importlib.util.spec_from_file_location('owned_bootstrap_capture_test', path)
    module = importlib.util.module_from_spec(spec)
    with patch.dict(sys.modules, {'norustc': tracing, 'tracedecay': watching}):
        spec.loader.exec_module(module)
    return module


BOOTSTRAP = load(Path(__file__).parent / 'dist' / 'owned_macro_server.py')


@unittest.skipUnless(os.name == 'posix', 'the owned runtime harness uses Unix sessions')
class CaptureTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.work = Path(self.tmp.name)
        self.trace = self.work/'bootstrap.exec'
        Watchdog.verdict = {}
        Watchdog.made.clear()

    def run_child(self, source, timeout=5):
        return BOOTSTRAP.guarded_run(
            [sys.executable, '-I', '-S', '-c', source], self.trace, cwd=self.work,
            env=dict(os.environ), blaze=Path(sys.executable), memory_mib=8192,
            timeout=timeout,
        )

    def evidence(self):
        report = Path(str(self.trace)+'.log').read_text()
        row = json.loads(Path(str(self.trace)+'.watchdog.json').read_text())
        self.assertEqual(Watchdog.made[0].limits, (8192, 3072))
        self.assertTrue(Watchdog.made[0].stop.is_set())
        return report, row

    def test_watchdog_refusal_keeps_both_streams_and_remains_a_failure(self):
        Watchdog.verdict = {'watchdog': 'swap floor violated'}
        with self.assertRaisesRegex(AssertionError, 'swap floor violated'):
            self.run_child("import sys; print('progress'); print('allocator live=123', file=sys.stderr)")
        report, row = self.evidence()
        self.assertIn('progress\n', report)
        self.assertIn('allocator live=123\n', report)
        self.assertIn('watchdog', row)

    def test_timeout_keeps_final_stderr_and_does_not_pass(self):
        with self.assertRaisesRegex(AssertionError, 'timed out'):
            self.run_child("import sys,time; print('before hang',file=sys.stderr,flush=True); time.sleep(60)", timeout=0.3)
        report, row = self.evidence()
        self.assertIn('before hang\n', report)
        self.assertTrue(row['timed_out'])
        self.assertNotEqual(row['code'], 0)

    def test_nonzero_compiler_status_is_preserved(self):
        result = self.run_child("import sys; print('diagnostic',file=sys.stderr); sys.exit(17)")
        report, row = self.evidence()
        self.assertEqual((result.returncode, row['code']), (17,17))
        self.assertIn('diagnostic\n', report)

    def test_success_output_is_unchanged(self):
        result = self.run_child("import sys; print('answer'); print('trace',file=sys.stderr)")
        report, row = self.evidence()
        self.assertEqual((result.returncode, result.stdout, result.stderr), (0,'answer\n','trace\n'))
        self.assertFalse(row['timed_out'])
        self.assertIn('answer\ntrace\n', report)

    def test_non_utf8_diagnostics_cannot_hide_failure(self):
        Watchdog.verdict = {'watchdog': 'memory limit'}
        with self.assertRaises(AssertionError):
            self.run_child("import os; os.write(2,b'last: \\xff\\n')")
        report, _ = self.evidence()
        self.assertIn('last: \ufffd\n', report)


if __name__ == '__main__':
    unittest.main()
