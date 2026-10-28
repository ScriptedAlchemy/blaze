#!/usr/bin/env python3
"""memgrowth_test.py: what `memgrowth.py` must refuse, with every probe mocked (no daemon, no
build): the M1 line is fitted to the samples the arm records, the unedited program is the
baseline, and an arm ends on a build that fails or that cargo made, on a program that is not
there or says something else, wherever in the arm that is, the last undoing included.

Run it as `python3 harness/memgrowth_test.py`.
"""
import contextlib, io, os, shutil, sys, tempfile, unittest
from types import SimpleNamespace
from unittest import mock

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import memgrowth as m  # noqa: E402


class Side:
    def cargo(self, *args, **kw):
        return 0, SimpleNamespace(returncode=0, stderr="")


class Sampler:
    peak_mb = 0

    def __enter__(self):
        return self

    def __exit__(self, *a):
        pass

    def add(self, *a):
        pass


def sample(pair, allocated=None):
    alloc = {} if allocated is None else {"allocated": allocated}
    return {"pair": pair, "alloc": alloc, "interned": {}, "own": {}, "holders": [], "pages": {}}


OK = SimpleNamespace(returncode=0, stderr="")

# What a worker logs of two collections of the interned types, among its other lines.
LOGGED = ("edit-to-binary: 12\n"
          "reclaim-types-us: 840 (20000 interned values before, 17000 after)\n"
          "check-us: 3\n"
          "reclaim-types-us: 610 (17100 interned values before, 17000 after)\n")


class Arm(unittest.TestCase):
    """`one_arm` with the daemon, the builds and the program replaced. `steps` is the list of
    the builds in order: pair 1's edit, pair 1's undoing, the edit after the last pair, and its
    undoing. `script` says what each build and each run of the program is; a build is
    `(returncode, built)` and a run is what `run_program` returns."""

    def setUp(self):
        self.work = tempfile.mkdtemp(prefix="memgrowth-test-")
        self.addCleanup(shutil.rmtree, self.work, ignore_errors=True)
        self.builds, self.runs = [], []
        self.build_script = [(0, "daemon")] * 4
        self.run_script = [(0, "hello\n")] * 5
        patches = [
            mock.patch.object(m, "start", lambda *a: (Side(), SimpleNamespace(pid=lambda: None), {}, 0, 0, [])),
            mock.patch.object(m.memory, "Sampler", Sampler),
            mock.patch.object(m, "wait_base", lambda *a: (0, "mock")),
            mock.patch.object(m, "figure", lambda *a, **kw: {
                "holders": [], "pages": {}, "alloc": {}, "interned": {}, "own": {}}),
            mock.patch.object(m, "edit", self.edit),
            mock.patch.object(m, "run_program", self.run_program),
            mock.patch.object(m, "logs", lambda daemon: LOGGED),
        ]
        for patch in patches:
            patch.start()
            self.addCleanup(patch.stop)

    def edit(self, side, daemon, spec, changes, back, timeout=300):
        at = len(self.builds)
        self.builds.append(back)
        code, built = self.build_script[at]
        return 0, SimpleNamespace(returncode=code, stderr="mock stderr"), built, ""

    def run_program(self, spec, ws):
        # Run number 0 is the unedited program's; after it, one run for each build.
        at = len(self.runs)
        self.runs.append(len(self.builds))
        return self.run_script[at]

    def arm(self, **spec):
        spec = dict({"ws": self.work}, **spec)
        with contextlib.redirect_stdout(io.StringIO()):
            return m.one_arm(spec, self.work, "test", "1", 1, 1, ["body"], [],
                             {"env": {}, "run": 1, "edits": {"body": []}, "recycle": False})

    def refused(self, why):
        with self.assertRaises(SystemExit) as raised:
            self.arm()
        self.assertIn(why, str(raised.exception))
        return raised.exception

    def test_an_arm_that_is_right_everywhere_counts_what_it_checked(self):
        got = self.arm()
        self.assertEqual(self.builds, [False, True, False, True])
        # The unedited program, then one run after each of the four builds.
        self.assertEqual(self.runs, [0, 1, 2, 3, 4])
        self.assertEqual(got["checked"], {"builds": 4, "runs": 4})

    def test_an_arm_records_the_time_of_each_collection_its_worker_logged(self):
        self.assertEqual(self.arm()["collections_us"], [840, 610])

    def test_the_final_undoing_is_checked_for_its_build_failing(self):
        self.build_script[3] = (1, "daemon")
        self.refused("the undoing of the edit after the last pair: the build failed (exit 1)")

    def test_the_final_undoing_is_checked_for_cargo_having_built_it(self):
        self.build_script[3] = (0, "cargo")
        self.refused("the undoing of the edit after the last pair: built by cargo")

    def test_the_final_undoing_is_checked_for_what_the_program_says(self):
        self.run_script[4] = (0, "something else\n")
        self.refused("the undoing of the edit after the last pair: the program exits 0 and says "
                     "'something else\\n'")

    def test_the_final_edit_is_checked_for_what_the_program_says(self):
        self.run_script[3] = (3, "hello\n")
        self.refused("the edit after the last pair: the program exits 3")

    def test_a_pair_is_checked_for_cargo_having_built_it(self):
        self.build_script[1] = (0, "cargo")
        self.refused("pair 1 (body), back: built by cargo")

    def test_a_program_that_is_not_there_at_the_last_step_ends_the_arm(self):
        self.run_script[4] = (None, "the build wrote no program")
        self.refused("the undoing of the edit after the last pair: the program did not run: "
                     "the build wrote no program")

    def test_a_program_that_is_not_there_at_any_step_is_not_accepted(self):
        # Every run failing the same way is not the program saying what it said: the verifier
        # found `one_arm` accepting it.
        self.run_script = [(None, "the build wrote no program")] * 5
        self.refused("the unedited program did not run: the build wrote no program")
        self.assertEqual(self.builds, [], "no edit is made to a program that does not run")

    def test_a_program_that_is_not_there_after_the_first_edit_is_not_accepted(self):
        self.run_script[1:] = [(None, "the build wrote no program")] * 4
        self.refused("pair 1 (body): the program did not run")

    def test_the_unedited_program_is_the_baseline_not_the_first_edited_one(self):
        # The first edit changes what the program says: held to the first edited program, as
        # the arm once was, that is the baseline and nothing is wrong.
        self.run_script = [(0, "hello\n")] + [(0, "changed\n")] * 4
        self.refused("pair 1 (body): the program exits 0 and says 'changed\\n', not 0 and 'hello\\n'")

    def test_an_unedited_program_that_fails_is_no_baseline(self):
        self.run_script[0] = (101, "")
        self.refused("the unedited program exits 101, not 0")

    def test_a_project_may_say_what_its_program_exits_with(self):
        self.run_script = [(2, "usage\n")] * 5
        with self.assertRaises(SystemExit):
            self.arm()
        self.run_script = [(2, "usage\n")] * 5
        self.builds.clear()
        self.runs.clear()
        got = self.arm(exit=2)
        self.assertEqual(got["checked"], {"builds": 4, "runs": 4})


class Line(unittest.TestCase):
    """M1's line is fitted to the samples as they are recorded: the allocator's figures are
    under `alloc`."""

    def test_four_samples_make_a_line(self):
        samples = [sample(n, n * 100000) for n in (1, 2, 3, 4)]
        got = m.fit(samples, "alloc", "allocated")
        self.assertEqual((got["n"], got["slope"], got["r2"]), (4, 100000.0, 1.0))

    def test_the_leak_rule_reads_the_recorded_samples(self):
        samples = [sample(n, 50_000_000 + n * 100000) for n in (1, 2, 3, 4)]
        rule = m.leak_rule({"arm": [{"samples": samples}]})
        self.assertEqual(rule["arm"]["slope_per_pair"], 100000.0)
        # 100,000 bytes a pair over 1,000 pairs is twice the ready size.
        self.assertTrue(rule["arm"]["leak"])

    def test_a_flat_line_is_no_leak(self):
        samples = [sample(n, 50_000_000) for n in (1, 2, 3, 4)]
        rule = m.leak_rule({"arm": [{"samples": samples}]})
        self.assertFalse(rule["arm"]["leak"])

    def test_a_line_with_no_first_figure_to_measure_it_against_is_unknown_not_no_leak(self):
        # The first sample has no allocator figure and the three after it make a line: there
        # is nothing to say what share of the ready size it adds.
        samples = [sample(1)] + [sample(n, n * 1000) for n in (2, 3, 4)]
        rule = m.leak_rule({"arm": [{"samples": samples}]})["arm"]
        self.assertEqual((rule["slope_per_pair"], rule["r2"]), (1000.0, 1.0))
        self.assertIsNone(rule["leak"])
        self.assertIsNone(rule["over_1000"])
        self.assertIn("first sample", rule["unknown"])

    def test_one_run_with_no_first_figure_makes_the_arm_unknown(self):
        known = [sample(n, 50_000_000) for n in (1, 2, 3, 4)]
        missing = [sample(1)] + [sample(n, 50_000_000 + n * 100000) for n in (2, 3, 4)]
        rule = m.leak_rule({"arm": [{"samples": known}, {"samples": missing}]})["arm"]
        self.assertIsNone(rule["leak"])

    def test_fewer_than_three_samples_or_no_figures_make_none(self):
        self.assertIsNone(m.fit([sample(1, 1), sample(2, 2)], "alloc", "allocated"))
        self.assertIsNone(m.fit([sample(n) for n in (1, 2, 3, 4)], "alloc", "allocated"))
        self.assertEqual(m.leak_rule({"arm": [{"samples": [sample(1, 1), sample(2, 2)]}]}), {})


class Medians(unittest.TestCase):
    def test_a_figure_of_zero_is_a_figure(self):
        # `retained` is 0 after a purge: it is the measurement, not a missing one.
        end = {"pages": {"VmRSS": 0}, "alloc": {"allocated": 10, "retained": 0},
               "interned": {"TyInterned": 0}, "own": {"checkouts": 0}}
        run = {"reclaim": "1", "end": end}
        got = m.medians({"arm": [run, run]})["arm"]
        self.assertEqual(got["end_retained"], 0)
        self.assertEqual(got["end_VmRSS"], 0)
        self.assertEqual(got["end_allocated"], 10)

    def test_the_collections_of_every_run_are_put_together(self):
        end = {"pages": {}, "alloc": {}, "interned": {}, "own": {}}
        runs = [{"reclaim": "1", "end": end, "collections_us": [840, 610]},
                {"reclaim": "1", "end": end, "collections_us": [700]}]
        got = m.medians({"arm": runs})["arm"]
        self.assertEqual(got["collections"], 3)
        self.assertEqual(got["collection_us"], {"median": 700, "min": 610, "max": 840})
        none = m.medians({"arm": [{"reclaim": "0", "end": end, "collections_us": []}] * 2})["arm"]
        self.assertEqual((none["collections"], none["collection_us"]), (0, None))


class Collections(unittest.TestCase):
    def test_only_the_lines_of_a_collection_are_read(self):
        self.assertEqual(m.collections(LOGGED), [840, 610])
        self.assertEqual(m.collections("tests-reach-us: 5\n"), [])


if __name__ == "__main__":
    unittest.main()
