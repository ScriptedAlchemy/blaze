#!/usr/bin/env python3
"""Exercise isolated metadata refresh and the lifetime of its graph commands."""
import asyncio
from contextlib import redirect_stderr
from pathlib import Path
import hashlib
import importlib.util
import io
import json
import os
import signal
import subprocess
import sys
import tempfile
import time
import tomllib
import unittest
from unittest.mock import patch

import refresh_nix_graphs as refresh

SPEC = importlib.util.spec_from_file_location("unit_graph", refresh.ROOT / "tools/nix/unit_graph.py")
graph = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(graph)


class GraphProcessTests(unittest.IsolatedAsyncioTestCase):
    async def asyncSetUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.directory = Path(self.temp.name)
        self.children = []
        self.descendants = []
        self.real_popen = subprocess.Popen

        def capture(argv, *args, **kwargs):
            process = self.real_popen(argv, *args, **kwargs)
            if argv[0] == sys.executable:
                self.children.append(process)
            return process

        self.capture = capture
        self.spawn_patch = patch.object(graph.subprocess, "Popen", side_effect=capture)
        self.spawn_patch.start()
        self.bound_patch = patch.object(graph, "CLEANUP_SECONDS", 0.25)
        self.bound_patch.start()

    async def asyncTearDown(self):
        self.spawn_patch.stop()
        self.bound_patch.stop()
        # A broken implementation may have reaped a launcher too early. Its known
        # fixture descendant has a stop file, so cleanup never signals a reused PGID.
        for _, stop in self.descendants:
            stop.touch()
        # A failed oracle must still clean a regression's leaked fixture children.
        # Only unreaped leaders are signalled, so their process-group IDs stay ours.
        for process in self.children:
            if process.returncode is None:
                try:
                    os.killpg(process.pid, signal.SIGKILL)
                except ProcessLookupError:
                    # A regression may omit start_new_session. Kill only our
                    # still-unreaped direct child, never its inherited group.
                    process.kill()
                process.wait(timeout=2)
        remaining = {int(path.read_text()) for path, _ in self.descendants if path.exists()}
        deadline = time.monotonic() + 2
        while remaining:
            rows = subprocess.run(["ps", "-A", "-o", "pid=", "-o", "stat="],
                                  capture_output=True, text=True, timeout=2, check=True).stdout.splitlines()
            remaining.intersection_update(int(row.split()[0]) for row in rows
                                          if row.split()[1][0] not in ("Z", "X"))
            if remaining and time.monotonic() >= deadline:
                self.fail(f"fixture descendants did not stop: {remaining}")
            if remaining:
                time.sleep(0.005)
        self.temp.cleanup()

    def command(self, source, *args):
        return [sys.executable, "-c", source, *map(str, args)]

    def assert_reaped(self):
        self.assertTrue(self.children)
        for process in self.children:
            self.assertIsNotNone(process.returncode, f"child {process.pid} remains owned and live")
            with self.assertRaises(ChildProcessError):
                os.waitpid(process.pid, os.WNOHANG)

    async def ready(self, path):
        async with asyncio.timeout(5):
            while not path.exists():
                await asyncio.sleep(0.005)

    async def test_success_keeps_stdout_cwd_and_environment_and_reaps(self):
        command = self.command("import os,sys; sys.stderr.write('diagnostic'); "
                               "sys.stdout.write('x' * 131072 + os.getcwd() + os.environ['GRAPH_VALUE'])")
        result = await graph.run_step("success", command, 5, self.directory, {"GRAPH_VALUE": "value"})
        self.assertEqual(result, "x" * 131072 + str(self.directory) + "value")
        self.assert_reaped()

    async def test_missing_waitable_group_support_refuses_before_spawning(self):
        with patch.object(graph.os, "WNOWAIT", create=True):
            del graph.os.WNOWAIT
            with self.assertRaisesRegex(graph.StepError, "requires waitid/WNOWAIT"):
                await graph.run_step("unsupported", self.command("pass"), 5, self.directory)
        self.assertEqual(self.children, [])

    async def test_original_exit_and_stderr_tail_survive_cleanup_failure(self):
        cleanup = graph.cleanup_step

        def failed_observation(process):
            cleanup(process)
            raise graph.StepError("injected observation failure")

        command = self.command("import sys; [print(i, file=sys.stderr) for i in range(35)]; sys.exit(23)")
        diagnostics = io.StringIO()
        with patch.object(graph, "cleanup_step", side_effect=failed_observation), redirect_stderr(diagnostics):
            with self.assertRaises(graph.StepError) as raised:
                await graph.run_step("original", command, 5, self.directory)
        self.assertEqual(str(raised.exception),
                         f"step 'original' failed\n  command: {' '.join(command)}\n  exit code: 23\n  stderr tail:\n"
                         + "\n".join(map(str, range(5, 35))))
        self.assertIn("step 'original' cleanup failed: injected observation failure", diagnostics.getvalue())
        self.assert_reaped()

    async def test_timeout_reaps_the_child_before_reporting(self):
        before = time.monotonic()
        with self.assertRaisesRegex(graph.StepError, "step 'timeout' timed out"):
            await graph.run_step("timeout", self.command("import time; time.sleep(60)"), 0.1, self.directory)
        self.assertLess(time.monotonic() - before, 3)
        self.assert_reaped()

    async def test_fetch_graphs_cancels_and_reaps_all_four_siblings(self):
        first = next(iter(graph.TARGETS.values()))

        def cargo(argv, *args, **kwargs):
            if argv[0] != "cargo":
                return self.capture(argv, *args, **kwargs)
            if argv[1] == "fetch":
                command = self.command("pass")
            elif argv[1] == "build" and first in argv:
                command = self.command(
                    "import pathlib,sys,time; root=pathlib.Path(sys.argv[1]);\n"
                    "while len(list(root.glob('ready-*'))) != 4: time.sleep(0.005)\n"
                    "print('primary graph failure', file=sys.stderr); sys.exit(23)", self.directory)
            else:
                name = "metadata" if argv[1] == "metadata" else argv[argv.index("--target") + 1]
                command = self.command("import pathlib,sys,time; pathlib.Path(sys.argv[1]).touch(); time.sleep(60)",
                                       self.directory / f"ready-{name}")
            return self.capture(command, *args, **kwargs)

        with patch.object(graph.subprocess, "Popen", side_effect=cargo), \
                patch.object(graph, "toolchain_prefix", return_value=[]):
            with self.assertRaises(ExceptionGroup) as raised:
                async with asyncio.timeout(5):
                    await graph.fetch_graphs(self.directory, ["-p", "missing-fixture"])
        failures = raised.exception.exceptions
        self.assertEqual(len(failures), 1)
        self.assertIsInstance(failures[0], graph.StepError)
        self.assertIn("exit code: 23", str(failures[0]))
        self.assertTrue(str(failures[0]).endswith("primary graph failure"))
        self.assertEqual(len(self.children), 6)  # fetch, four graphs, metadata
        self.assertEqual(sum(process.returncode < 0 for process in self.children), 4)
        self.assert_reaped()

    async def test_cancellation_during_spawn_keeps_the_new_child_owned(self):
        def cancel_after_creation(argv, *args, **kwargs):
            process = self.capture(argv, *args, **kwargs)
            if argv[0] == sys.executable:
                asyncio.current_task().cancel("after child creation, before handle return")
            return process

        with patch.object(graph.subprocess, "Popen", side_effect=cancel_after_creation):
            task = asyncio.create_task(graph.run_step("spawn", self.command("import time; time.sleep(60)"),
                                                     5, self.directory))
            with self.assertRaises(asyncio.CancelledError):
                await task
        self.assert_reaped()

    async def test_repeated_cancellation_cannot_interrupt_group_cleanup(self):
        ready = self.directory / "ready"
        command = self.command("import pathlib,signal,sys,time; signal.signal(signal.SIGTERM, signal.SIG_IGN); "
                               "pathlib.Path(sys.argv[1]).touch(); time.sleep(60)", ready)
        task = asyncio.create_task(graph.run_step("repeat", command, 5, self.directory))
        await self.ready(ready)
        live_group = graph.live_group
        repeated = []

        def cancel_again(process, seconds):
            if not repeated:
                repeated.append(process.pid)
                asyncio.current_task().cancel("second cancellation during cleanup")
            return live_group(process, seconds)

        before = time.monotonic()
        with patch.object(graph, "live_group", side_effect=cancel_again):
            task.cancel("first cancellation")
            with self.assertRaises(asyncio.CancelledError):
                await task
        self.assertEqual(repeated, [self.children[0].pid])
        self.assertEqual(self.children[0].returncode, -signal.SIGKILL)
        self.assertLess(time.monotonic() - before, 3)
        self.assert_reaped()

    async def test_exited_launcher_cannot_leave_a_descendant_or_touch_another_group(self):
        unrelated = self.capture(self.command("import time; time.sleep(60)"), start_new_session=True)
        for status in (0, 23):
            with self.subTest(status=status):
                pid_file = self.directory / f"descendant-{status}"
                descendant_ready = self.directory / f"descendant-ready-{status}"
                descendant_stop = self.directory / f"descendant-stop-{status}"
                self.descendants.append((pid_file, descendant_stop))
                descendant = ("import pathlib,signal,sys,time; signal.signal(signal.SIGTERM, signal.SIG_IGN); "
                              "pathlib.Path(sys.argv[1]).touch(); stop=pathlib.Path(sys.argv[2]);\n"
                              "while not stop.exists(): time.sleep(0.005)")
                launcher = ("import pathlib,subprocess,sys,time; "
                            "child=subprocess.Popen([sys.executable, '-c', sys.argv[1], sys.argv[3], sys.argv[5]]); "
                            "pathlib.Path(sys.argv[2]).write_text(str(child.pid)); "
                            "ready=pathlib.Path(sys.argv[3]);\n"
                            "while not ready.exists(): time.sleep(0.005)\n"
                            "print('launcher failed', file=sys.stderr); sys.exit(int(sys.argv[4]))")
                command = self.command(launcher, descendant, pid_file, descendant_ready, status, descendant_stop)
                with self.assertRaises(graph.StepError) as raised:
                    await graph.run_step("descendant", command, 5, self.directory)
                if status:
                    self.assertIn("exit code: 23", str(raised.exception))
                    self.assertTrue(str(raised.exception).endswith("launcher failed"))
                else:
                    self.assertIn("left live processes after it exited", str(raised.exception))
                pid = int(pid_file.read_text())
                rows = subprocess.run(["ps", "-A", "-o", "pid=", "-o", "stat="],
                                      capture_output=True, text=True, timeout=2, check=True).stdout.splitlines()
                self.assertFalse([row for row in rows if int(row.split()[0]) == pid
                                  and row.split()[1][0] not in ("Z", "X")], f"descendant {pid} survived")
                self.assertIsNone(unrelated.poll(), "cleanup touched an unrelated process group")
        os.killpg(unrelated.pid, signal.SIGKILL)
        unrelated.wait(timeout=2)
        self.assert_reaped()

    async def test_a_failed_group_observation_still_kills_and_reaps(self):
        with patch.object(graph, "live_group", side_effect=OSError("injected ps failure")), \
                redirect_stderr(io.StringIO()) as diagnostics:
            with self.assertRaisesRegex(graph.StepError, "step 'timeout' timed out"):
                await graph.run_step("timeout", self.command("import time; time.sleep(60)"), 0.1, self.directory)
        self.assertIn("cleanup failed: injected ps failure", diagnostics.getvalue())
        self.assert_reaped()

    async def test_group_exit_between_snapshot_and_signal_is_reaped(self):
        release = self.directory / "release"
        process = self.capture(self.command("import pathlib,sys,time; path=pathlib.Path(sys.argv[1]);\n"
                                           "while not path.exists(): time.sleep(0.005)", release),
                               start_new_session=True)
        raced = []

        def exited_before_signal(group, signum):
            self.assertEqual(group, process.pid)
            raced.append(signum)
            release.touch()
            deadline = time.monotonic() + 2
            while graph.observed_exit(process) is None:
                if time.monotonic() >= deadline:
                    self.fail("fixture child did not exit at the injected signal boundary")
                time.sleep(0.005)
            raise ProcessLookupError("group exited between observation and signal")

        with patch.object(graph.os, "killpg", side_effect=exited_before_signal):
            self.assertFalse(graph.cleanup_step(process))
        self.assertEqual(raced, [signal.SIGTERM])
        self.assertEqual(process.returncode, 0)
        self.assert_reaped()


class RefreshTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name) / "repo"
        self.root.mkdir()
        self.out = Path(self.temp.name) / "artifact"
        self.git("init", "-q")
        for directory in (self.root, self.root / "demo/life"):
            directory.mkdir(parents=True, exist_ok=True)
            (directory / "Cargo.toml").write_text('[package]\nname="fixture"\nversion="0.1.0"\n')
            (directory / "Cargo.lock").write_text('version=4\n[[package]]\nname="fixture"\nversion="0.1.0"\n')
        (self.root / "untouched").write_text("original\n")
        (self.root / "flake.nix").write_text("{}\n")
        (self.root / "flake.lock").write_text("{}\n")
        (self.root / "pins").mkdir()
        (self.root / "pins/toolchain").write_text("nightly-2026-10-27\n")
        self.graphs(self.root)
        self.git("add", ".")
        self.git("-c", "user.name=fixture", "-c", "user.email=fixture@example.invalid", "commit", "-qm", "fixture")
        self.commands = []

    def git(self, *args):
        return refresh.run(["git", *args], self.root)

    def graphs(self, work):
        for project, directory in (("life", work / "demo/life"),):
            graph = {
                "version": 4,
                "manifest": tomllib.loads((directory / "Cargo.toml").read_text()),
                "lock": tomllib.loads((directory / "Cargo.lock").read_text()),
                "packages": {"fixture@0.1.0": {}},
                "systems": {name: {"units": {"unit": {}}, "roots": ["unit"]} for name in refresh.SYSTEMS},
            }
            target = work / f"tools/nix/units/{project}.json"
            target.parent.mkdir(parents=True, exist_ok=True)
            target.write_text(json.dumps(graph) + "\n")

    def metadata(self, command, cwd):
        self.commands.append((command, cwd))
        if command[0] == "git":
            return refresh.run(command, cwd)
        self.assertEqual(command[:4], ["nix", "develop", str(self.root), "--command"])
        self.assertNotEqual(cwd, self.root)
        if command[4] == "cargo":
            self.assertEqual(command[4:], ["cargo", "metadata", "--format-version", "1", "--manifest-path", "demo/life/Cargo.toml"])
            (cwd / "demo/life/Cargo.lock").write_text('version=4\n[[package]]\nname="fixture"\nversion="0.1.1"\n')
        else:
            self.assertEqual(command[4:], ["python3", "tools/nix/unit_graph.py"])
            self.graphs(cwd)
        return b""

    def test_export_is_bound_to_committed_source_and_leaves_checkout_untouched(self):
        # The ordinary graph check may have already changed a generated file before failing.
        original = self.root / "tools/nix/units/life.json"
        original.write_text("dirty output from failed gate\n")
        before = self.git("diff", "--binary")
        refresh.refresh(self.root, refresh.Request(self.out), self.metadata)
        # The real CI checkout may be shallow; the detached worktree must
        # never become the Nix flake input that asks Git for revCount.
        nix = [(cmd, cwd) for cmd, cwd in self.commands if cmd[0] == "nix"]
        self.assertEqual([cmd[4] for cmd, _ in nix], ["cargo", "python3"])
        self.assertTrue(all(cmd[2] == str(self.root) and cwd != self.root for cmd, cwd in nix))
        receipt = json.loads((self.out / "source.json").read_text())
        self.assertEqual(receipt["source_commit"], self.git("rev-parse", "HEAD").decode().strip())
        self.assertEqual(receipt["source_tree"], self.git("rev-parse", "HEAD^{tree}").decode().strip())
        self.assertEqual(self.git("diff", "--binary"), before)
        self.assertEqual(self.git("worktree", "list", "--porcelain").count(b"worktree "), 1)
        for name, digest in receipt["files"].items():
            self.assertEqual(hashlib.sha256((self.out / name).read_bytes()).hexdigest(), digest)
        self.assertIn(b"demo/life/Cargo.lock", (self.out / "refresh.patch").read_bytes())
        self.assertNotIn("dirty output", (self.out / "tools/nix/units/life.json").read_text())
        refresh.checked_graphs(self.out_with_manifests())

    def out_with_manifests(self):
        for name in ("Cargo.toml", "Cargo.lock", "demo/life/Cargo.toml"):
            (self.out / name).write_bytes((self.root / name).read_bytes())
        return self.out

    def test_bad_regeneration_never_gets_an_export_receipt(self):
        for fault in ("unrelated", "mismatched", "missing-platform", "process"):
            with self.subTest(fault=fault):
                def faulty(command, cwd):
                    if command[0] == "git" or (command[0] == "nix" and command[4] == "cargo"):
                        return self.metadata(command, cwd)
                    if fault == "process":
                        raise subprocess.CalledProcessError(1, command)
                    self.metadata(command, cwd)
                    if fault == "unrelated":
                        (cwd / "untouched").write_text("bad change\n")
                    else:
                        path = cwd / "tools/nix/units/life.json"
                        graph = json.loads(path.read_text())
                        if fault == "mismatched":
                            graph["lock"] = {}
                        else:
                            graph["systems"].pop("x86_64-darwin")
                        path.write_text(json.dumps(graph))
                    return b""
                with self.assertRaises((ValueError, subprocess.CalledProcessError)):
                    refresh.refresh(self.root, refresh.Request(self.out), faulty)
                self.assertFalse(self.out.exists())
                self.assertEqual(self.git("worktree", "list", "--porcelain").count(b"worktree "), 1)
                self.assertEqual(self.git("diff"), b"")

    def test_changed_shell_pin_refuses_metadata_export(self):
        (self.root / "pins/toolchain").write_text("nightly-2026-10-28\n")
        with self.assertRaisesRegex(ValueError, "pins/toolchain: checkout differs"):
            refresh.refresh(self.root, refresh.Request(self.out), self.metadata)
        self.assertFalse(self.out.exists())
        self.assertEqual(self.git("worktree", "list", "--porcelain").count(b"worktree "), 1)

    def test_request_and_destination_are_strict(self):
        for text in ('{}', '{"out":"relative"}', '{"out":null}', '{"out":"/tmp/a","skip_locked":true}', '[]'):
            with self.subTest(text=text), self.assertRaises(ValueError):
                refresh.Request.parse(text)
        self.assertEqual(refresh.Request.parse(json.dumps({"out": str(self.out)})).out, self.out)
        for out in (self.root, self.root / "output", Path(self.temp.name)):
            with self.subTest(out=out), self.assertRaises(ValueError):
                refresh.refresh(self.root, refresh.Request(out), self.metadata)
        self.assertEqual(self.commands, [])


if __name__ == "__main__":
    unittest.main()
