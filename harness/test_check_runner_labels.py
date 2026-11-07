#!/usr/bin/env python3
"""test_check_runner_labels.py: the cases harness/check_runner_labels.py is held to.

    python3 -m unittest discover --start-directory harness --pattern 'test_*.py'

The fmt job of .github/workflows/ra-jit.yaml runs it, with the check itself, so that the check
that no pull request's workflow can pick a self-hosted runner has cases of its own, and reads
every workflow of this repository.
"""
import glob, os, sys, unittest

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
import check_runner_labels as check

WORKFLOWS = os.path.join(os.path.dirname(HERE), ".github", "workflows")


class RunnerLabels(unittest.TestCase):
    def problems(self, text):
        return check.check("workflow.yaml", text)

    def test_hosted_runner(self):
        self.assertEqual(self.problems(
            "on:\n  pull_request:\njobs:\n  test:\n    runs-on: ubuntu-latest\n"), [])

    def test_bare_self_hosted(self):
        problems = self.problems("on: pull_request\njobs:\n  test:\n    runs-on: self-hosted\n")
        self.assertEqual(len(problems), 1)
        self.assertIn("self-hosted", problems[0])

    def test_self_hosted_in_a_list(self):
        # All three: `linux` and `x64` are the default labels a self-hosted runner also gets.
        problems = self.problems(
            "on: [push, pull_request]\njobs:\n  test:\n    runs-on: [self-hosted, linux, x64]\n")
        self.assertEqual(len(problems), 3)
        self.assertIn("'self-hosted'", " ".join(problems))

    def test_machine_labels(self):
        for label in ("bench-macos", "macos-15-runner"):
            problems = self.problems(f"on: pull_request\njobs:\n  test:\n    runs-on: {label}\n")
            self.assertEqual(len(problems), 1, label)
            self.assertIn(label, problems[0])

    def test_machine_label_without_a_pull_request(self):
        # A `push` or `schedule` workflow cannot be written by a fork, but a machine's label is
        # still nothing a job of this repository may name.
        problems = self.problems("on: schedule\njobs:\n  test:\n    runs-on: bench-macos\n")
        self.assertEqual(len(problems), 1)

    def test_generic_self_hosted_labels(self):
        # `linux` and `x64` are the labels a self-hosted runner gets by default.
        problems = self.problems(
            "on: pull_request\njobs:\n  test:\n    runs-on: [linux, x64]\n")
        self.assertEqual(len(problems), 2)

    def test_matrix_of_hosted_runners(self):
        self.assertEqual(self.problems(
            "on: pull_request\n"
            "jobs:\n"
            "  test:\n"
            "    strategy:\n"
            "      matrix:\n"
            "        os: [ubuntu-24.04, ubuntu-24.04-arm, macos-15]\n"
            "    runs-on: ${{ matrix.os }}\n"), [])

    def test_matrix_include_of_hosted_runners(self):
        self.assertEqual(self.problems(
            "on: pull_request\n"
            "jobs:\n"
            "  test:\n"
            "    strategy:\n"
            "      matrix:\n"
            "        include:\n"
            "          - { os: ubuntu-22.04, target: x86_64-unknown-linux-gnu }\n"
            "          - { os: macos-15, target: aarch64-apple-darwin }\n"
            "    runs-on: ${{ matrix.os }}\n"), [])

    def test_matrix_that_can_be_self_hosted(self):
        problems = self.problems(
            "on: pull_request\n"
            "jobs:\n"
            "  test:\n"
            "    strategy:\n"
            "      matrix:\n"
            "        os: [ubuntu-24.04, self-hosted]\n"
            "    runs-on: ${{ matrix.os }}\n")
        self.assertEqual(len(problems), 1)

    def test_matrix_this_cannot_resolve(self):
        # A matrix computed by another job could hold anything.
        problems = self.problems(
            "on: pull_request\n"
            "jobs:\n"
            "  test:\n"
            "    strategy:\n"
            "      matrix: ${{ fromJSON(needs.platforms.outputs.matrix) }}\n"
            "    runs-on: ${{ matrix.label }}\n")
        self.assertEqual(len(problems), 1)

    def test_runner_given_to_a_reusable_workflow(self):
        problems = self.problems(
            "on: pull_request\n"
            "jobs:\n"
            "  test:\n"
            "    uses: ./.github/workflows/platform.yaml\n"
            "    with:\n"
            "      os: self-hosted\n")
        self.assertEqual(len(problems), 1)

    def test_hosted_runner_given_to_a_reusable_workflow(self):
        self.assertEqual(self.problems(
            "on: pull_request\n"
            "jobs:\n"
            "  test:\n"
            "    uses: ./.github/workflows/platform.yaml\n"
            "    with:\n"
            "      os: macos-15\n"), [])

    def test_runtime_matrix_without_a_pull_request(self):
        # bench-readme's shape: the label comes from harness/benchplatforms.py at run time.
        self.assertEqual(self.problems(
            "on: schedule\n"
            "jobs:\n"
            "  bench:\n"
            "    runs-on: ${{ matrix.label }}\n"), [])

    def test_unreadable_workflow(self):
        with self.assertRaises(check.Unreadable):
            self.problems("on: push\njobs:\n\ttest:\n")

    def test_this_repository(self):
        paths = sorted(glob.glob(os.path.join(WORKFLOWS, "*.yaml")))
        self.assertTrue(paths)
        for path in paths:
            with open(path, encoding="utf-8") as f:
                self.assertEqual(check.check(path, f.read()), [], path)


if __name__ == "__main__":
    unittest.main()
