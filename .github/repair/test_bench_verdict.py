#!/usr/bin/env python3
"""The verdicts of `bench_verdict.py`, on tango's output as run 41131270465 of PR18 printed it."""
import importlib.util
from pathlib import Path
import unittest

spec = importlib.util.spec_from_file_location("bench_verdict", Path(__file__).with_name("bench_verdict.py"))
bench_verdict = importlib.util.module_from_spec(spec)
spec.loader.exec_module(bench_verdict)

NAMES = ["key/entry", "key/salted", "pack/encode_record_1k", "pack/check_1000x512", "store/get_hit", "store/has_miss"]


def run(slowed=(), measured=True):
    """Tango's `-v` text for the six benchmarks, with the named ones +40% and significant."""
    if not measured:
        return "Error: Unable to calculate results for a benchmark: store/get_hit\n"
    text = ""
    for name in NAMES:
        bad = name in slowed
        text += (
            f"{name}  (n: 300, outliers: 2)\n"
            "                          baseline       candidate               ∆\n"
            "                 ╭" + "─" * 48 + "\n"
            "    mean         │         96.7 ns        "
            + ("135.4 ns         38.7 ns   +40.02%*" if bad else " 95.1 ns         -1.6 ns    -1.56%")
            + "\n"
            "    min          │         95.8 ns         94.5 ns         -1.4 ns\n"
            "         cpu_usr │      3004673000      3004711000           38000    +0.00%\n\n"
        )
    return text


class VerdictTest(unittest.TestCase):
    def judge(self, aa, *attempts):
        verdict, report, status = bench_verdict.judge(aa, list(attempts), "an AMD EPYC 7763 (4 CPUs)", 21)
        return verdict, status, report

    def test_reads_only_significant_slowdowns(self):
        text = run(["store/has_miss"]).replace("-1.56%", "-1.56%*")
        self.assertEqual(set(bench_verdict.slow(text)), {"store/has_miss"})
        self.assertEqual(len(bench_verdict.means(text)), 6)

    def test_clean_run_passes(self):
        self.assertEqual(self.judge(run(), run())[:2], ("pass", 0))

    def test_the_same_benchmark_twice_is_a_regression(self):
        verdict, status, report = self.judge(run(), run(["store/has_miss"]), run(["store/has_miss"]))
        self.assertEqual((verdict, status), ("regression", 1))
        self.assertIn("over the threshold in both comparisons: store/has_miss", report)

    def test_different_benchmarks_in_each_comparison_is_noise(self):
        # Run 41131270465: pack/check in the first, store/has_miss in the second, one pair of binaries.
        self.assertEqual(self.judge(run(), run(["pack/check_1000x512"]), run(["store/has_miss"]))[:2], ("noise", 0))

    def test_a_slowdown_that_does_not_come_back_is_noise(self):
        self.assertEqual(self.judge(run(), run(["store/has_miss"]), run())[:2], ("noise", 0))

    def test_the_binary_slow_against_itself_is_environmental(self):
        verdict, status, report = self.judge(run(["store/has_miss"]), run(["store/has_miss"]), run(["store/has_miss"]))
        self.assertEqual((verdict, status), ("environmental", 0))
        self.assertIn("where it would have been regression", report)
        self.assertIn("store/has_miss +40.02% (96.7 ns -> 135.4 ns)", report)
        self.assertIn("an AMD EPYC 7763 (4 CPUs)", report)

    def test_a_control_that_measured_nothing_does_not_excuse_a_regression(self):
        verdict, status, report = self.judge(run(measured=False), run(["store/has_miss"]), run(["store/has_miss"]))
        self.assertEqual((verdict, status), ("regression", 1))
        self.assertIn("A/A control (bench-base against a copy of itself, 21 s): measured nothing", report)

    def test_measuring_nothing_in_the_last_comparison_fails(self):
        self.assertEqual(self.judge(run(), run(["store/has_miss"]), run(measured=False))[:2], ("error", 1))

    def test_an_error_in_the_first_comparison_is_remeasured(self):
        self.assertEqual(self.judge(run(), run(measured=False), run())[:2], ("pass", 0))

    def test_a_single_slow_comparison_is_not_confirmed_clean(self):
        self.assertEqual(self.judge(run(), run(["store/has_miss"]))[:2], ("regression", 1))

    def test_partial_final_report_cannot_pass(self):
        partial = run().split("store/has_miss")[0]
        self.assertEqual(self.judge(run(), partial)[:2], ("error", 1))
        self.assertEqual(self.judge(run(), run(["store/has_miss"]), partial)[:2], ("error", 1))

    def test_duplicate_and_truncated_blocks_are_not_measurements(self):
        self.assertEqual(self.judge(run(), run() + run())[:2], ("error", 1))
        truncated = run().split("store/has_miss")[0] + "store/has_miss (n: 300, outliers: 2)\n"
        self.assertEqual(self.judge(run(), truncated)[:2], ("error", 1))

    def test_incomplete_slow_control_cannot_excuse_regression(self):
        partial = run(["store/get_hit"]).split("store/has_miss")[0]
        self.assertEqual(self.judge(partial, run(["store/has_miss"]), run(["store/has_miss"]))[:2],
                         ("error", 1))

    def test_complete_retry_can_replace_failed_partial_attempt(self):
        partial = run().split("store/has_miss")[0]
        self.assertEqual(self.judge(run(), partial, run())[:2], ("pass", 0))

    def test_completed_text_does_not_hide_a_failed_process(self):
        for code in (1, 2, 137, -11):
            with self.subTest(code=code):
                verdict, _, status = bench_verdict.judge(run(), [run()], exit_codes=[code])
                self.assertEqual((verdict, status), ("error", 1))

    def test_failed_control_cannot_excuse_a_repeated_regression(self):
        verdict, _, status = bench_verdict.judge(run(["store/has_miss"]),
            [run(["store/has_miss"]), run(["store/has_miss"])],
            exit_codes=[1, 1], aa_exit_code=137)
        self.assertEqual((verdict, status), ("regression", 1))

    def test_valid_process_exit_codes_preserve_existing_policy(self):
        verdict, _, status = bench_verdict.judge(run(), [run(measured=False), run()],
                                                exit_codes=[2, 0], aa_exit_code=0)
        self.assertEqual((verdict, status), ("pass", 0))
        verdict, _, status = bench_verdict.judge(run(["store/has_miss"]),
            [run(["store/has_miss"]), run(["store/has_miss"])],
            exit_codes=[1, 1], aa_exit_code=1)
        self.assertEqual((verdict, status), ("environmental", 0))

    def test_cpu_and_digests(self):
        cpu = (
            "Model name:                              AMD EPYC 7763 64-Core Processor\n"
            "CPU(s):                                  4\n"
            "flags : fpu\n" + "a" * 64 + "  /tmp/bench-base\n" + "b" * 64 + "  /tmp/bench-head\n"
        )
        self.assertEqual(
            bench_verdict.cpu_line(cpu),
            "AMD EPYC 7763 64-Core Processor (4 CPUs); bench-base " + "a" * 12 + ", bench-head " + "b" * 12,
        )


if __name__ == "__main__":
    unittest.main()
