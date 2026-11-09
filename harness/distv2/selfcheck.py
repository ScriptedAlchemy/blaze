#!/usr/bin/env python3
"""Small planted-bug controls for the acceptance oracles; starts no Blaze processes."""
from __future__ import annotations

import hashlib
import importlib.util
import json
import os
import subprocess
from pathlib import Path
import sys
import tempfile
import unittest
from unittest.mock import patch

HARNESS = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(HARNESS))
from distv2 import evidence as e

spec = importlib.util.spec_from_file_location("distv2_harness", HARNESS / "distv2.py")
h = importlib.util.module_from_spec(spec)
sys.modules[spec.name] = h
spec.loader.exec_module(h)

KEY, OTHER, UNIT = "11" * 32, "22" * 32, "33" * 32


def row(**extra):
    return {"unit": "mathy", "unit_digest": UNIT, "source": "executed", "execution": 7,
            "attempt": 2, "joined": True, "grants": 1, "compiled_for_work": 1,
            "tail_compiled": 0, "stored_for_work": 0, "work_compiled_keys": [KEY],
            "tail_compiled_keys": [], **extra}


def grant(**extra):
    return {"event": "grant", "pid": 123, "unit_digest": UNIT, "execution": 7, "attempt": 2,
            "holder": "a", "generation": 3, "seq": 4, "work": [KEY, OTHER], **extra}


class Oracles(unittest.TestCase):
    def test_duplicate_tail_and_cross_daemon_compiles_are_caught(self):
        e.no_duplicate_compile([row(), row(work_compiled_keys=[OTHER])])
        with self.assertRaises(e.Failed):
            e.no_duplicate_compile([row(), row()])
        with self.assertRaises(e.Failed):
            e.compiled(row(tail_compiled=1, tail_compiled_keys=[KEY]))
        with self.assertRaises(e.Failed):
            e.compiled(row(compiled_for_work=0))
        missing = row()
        del missing["work_compiled_keys"]
        with self.assertRaises(e.Failed):
            e.compiled(missing)

    def test_local_static_repeats_preserve_counts_but_cannot_hide_cross_daemon_work(self):
        repeated = row(tail_compiled=2, tail_compiled_keys=[OTHER, OTHER])
        self.assertEqual(e.compiled(repeated), {KEY, OTHER})
        self.assertEqual(repeated["tail_compiled_keys"], [OTHER, OTHER])
        tail_only = row(compiled_for_work=0, work_compiled_keys=[], tail_compiled=2,
                        tail_compiled_keys=[OTHER, OTHER])
        self.assertEqual(e.compiled(tail_only), {OTHER})
        published = row(source="published", execution=-1, joined=False, grants=0,
                        compiled_for_work=0, work_compiled_keys=[])
        e.no_duplicate_compile([repeated, published])
        for bad in (row(compiled_for_work=2, work_compiled_keys=[KEY, KEY]),
                    {**repeated, "tail_compiled": 1}):
            with self.assertRaises(e.Failed):
                e.compiled(bad)
        for peer in (row(work_compiled_keys=[OTHER]),
                     row(execution=8, attempt=3, compiled_for_work=0, work_compiled_keys=[],
                         tail_compiled=1, tail_compiled_keys=[OTHER])):
            with self.assertRaises(e.Failed):
                e.no_duplicate_compile([repeated, peer])

    def test_recovery_requires_every_name_in_same_execution_under_new_lease(self):
        old = grant()
        new = grant(holder="b", generation=5)
        done = {**new, "event": "result_accepted"}
        self.assertTrue(e.recovered([old, new, done], old))
        # A matching acceptance logged before its replacement grant is not a recovery.
        self.assertFalse(e.recovered([old, done, new], old))
        self.assertFalse(e.recovered([old, new, done], old, accepted_after=3))
        with self.assertRaisesRegex(e.Failed, "outside its grant"):
            e.recovered([old, new, {**done, "work": [KEY, "44" * 32]}], old)
        self.assertFalse(e.recovered([], old))
        self.assertFalse(e.recovered([old, {**old, "event": "result_accepted"}], old))
        self.assertFalse(e.recovered([old, new, {**done, "work": [KEY]}], old))
        self.assertFalse(e.recovered([old, {**new, "execution": 8}, {**done, "execution": 8}], old))
        self.assertFalse(e.recovered([old, {**new, "unit_digest": OTHER}, done], old))

    def test_refusal_must_name_exact_stale_batch_and_real_reason(self):
        old = grant()
        refusal = {**old, "event": "result_refused", "refusal": "DeadLease"}
        self.assertTrue(e.stale_refused([refusal], old))
        # Earlier refusals cannot stand in for the resumed victim's submission.
        self.assertFalse(e.stale_refused([refusal], old, refused_after=1))
        self.assertTrue(e.stale_refused([refusal, refusal], old, refused_after=1))
        for field, wrong in (("holder", "b"), ("generation", 4), ("seq", 5), ("attempt", 3),
                             ("execution", 8), ("refusal", "Unknown")):
            self.assertFalse(e.stale_refused([{**refusal, field: wrong}], old), field)
        self.assertFalse(e.stale_refused([{**old, "event": "holder-lost"}], old))
        self.assertTrue(e.accepted([{**old, "event": "result_accepted"}], old))

    def test_cold_cache_rejects_no_evidence_or_any_work(self):
        cold = row(source="published", execution=-1, joined=False, grants=0,
                   compiled_for_work=0, work_compiled_keys=[])
        e.cached([cold])
        for bad in ([], [row()], [{**cold, "grants": 1}], [{**cold, "tail_compiled": 1, "tail_compiled_keys": [KEY]}]):
            with self.assertRaises(e.Failed):
                e.cached(bad)

    def test_native_manifest_corruption_and_current_attempt_controls(self):
        with tempfile.TemporaryDirectory() as directory:
            blobs = Path(directory)
            (blobs / "packs").mkdir()
            (blobs / "manifests").mkdir()
            # Synthetic oracle input, never used as acceptance evidence.
            raw = b"blzdpk\0\x01" + bytes.fromhex(KEY) + b"\1" + (1).to_bytes(8, "little") + b"x"
            name = hashlib.sha256(raw).hexdigest()
            target = blobs / "packs" / f"{name}.pack"
            target.write_bytes(raw)
            manifest = blobs / "manifests" / UNIT
            manifest.write_text(f"{name} {len(raw)} {e.pack_keys_digest({KEY})}\n")
            event = {"event": "published", "unit_digest": UNIT, "execution": 7, "attempt": 2, "packs": [name]}
            done = {"event": "done", "execution": 7, "attempt": 2, "outcome": "Passed"}
            e.publication(blobs, [row()], [event, done])
            for bad in ([], [event], [done, event], [{**event, "attempt": 1}, done], [{**event, "packs": []}, done]):
                with self.assertRaises(e.Failed):
                    e.publication(blobs, [row()], bad)
            with self.assertRaises(e.Failed):
                e.publication(blobs, [row(work_compiled_keys=[OTHER])], [event, done])
            target.write_bytes(raw[:-1] + b"y")
            with self.assertRaises(e.Failed):
                e.manifest(blobs, UNIT)
            target.write_bytes(raw)
            e.publication(blobs, [row()], [event, done])
            manifest.unlink()
            with self.assertRaises(e.Failed):
                e.manifest(blobs, UNIT)

    def test_native_manifest_checks_key_digests_and_immutable_values_across_packs(self):
        with tempfile.TemporaryDirectory() as directory:
            blobs = Path(directory)
            (blobs / "packs").mkdir()
            (blobs / "manifests").mkdir()
            manifest = blobs / "manifests" / UNIT

            def put(pairs):
                raw = bytearray(b"blzdpk\0\x01")
                for key, payload in sorted(pairs.items()):
                    raw.extend(bytes.fromhex(key))
                    raw.extend(b"\x01")
                    raw.extend(len(payload).to_bytes(8, "little"))
                    raw.extend(payload)
                raw = bytes(raw)
                name = hashlib.sha256(raw).hexdigest()
                (blobs / "packs" / f"{name}.pack").write_bytes(raw)
                return f"{name} {len(raw)} {e.pack_keys_digest(set(pairs))}"

            first = put({KEY: b"first"})
            conflict = put({KEY: b"different bytes under the same immutable key"})
            manifest.write_text(first + "\n" + conflict + "\n")
            with self.assertRaisesRegex(e.Failed, "different values across published packs"):
                e.manifest(blobs, UNIT)

            # Repeating the same immutable value in differently packed files is allowed.
            overlap = put({KEY: b"first", OTHER: b"an independent value"})
            manifest.write_text(first + "\n" + overlap + "\n")
            packs, keys = e.manifest(blobs, UNIT)
            self.assertEqual((len(packs), keys), (2, {KEY, OTHER}))

            # A pack's SHA-256 does not prove its separately committed key-set digest.
            fields = overlap.split()
            fields[2] = KEY
            manifest.write_text(first + "\n" + " ".join(fields) + "\n")
            with self.assertRaisesRegex(e.Failed, "PackRef.keys digest"):
                e.manifest(blobs, UNIT)

            self.assertEqual(e.pack_keys_digest({KEY}),
                             "847bea1103f30c14e239416b59d188feda5a64bcf4d9f7c591ade9df9a6530e1")
            self.assertEqual(e.pack_keys_digest({f"{n:02x}" * 32 for n in range(24)}),
                             "a46647a51115be67ab1dfdc94e7d0ec25d91e06dec049bc0ca9e94bab12a6327")

    def test_committed_pack_refusal_rejects_fallback_and_unrelated_failures(self):
        with tempfile.TemporaryDirectory() as directory:
            target = Path(directory) / f"{KEY}.pack"
            artifact = Path(directory) / "app"
            consumer = unittest.mock.Mock()
            consumer.program.return_value = artifact
            refused = (1, f"distributed build failed: committed pack {KEY} is corrupt")
            consumer.build.return_value = refused
            for policy in ("never", "fallback"):
                home = f"corrupt-{policy}"
                h.committed_pack_refused(consumer, home, policy, target)
                consumer.build.assert_called_with(home, BLAZE_DIST_LOCAL=policy)
            for bad in ((0, refused[1]), (1, "distributed build failed: unrelated"),
                        (1, f"ordinary build error on {KEY}")):
                consumer.build.return_value = bad
                with self.assertRaises(e.Failed):
                    h.committed_pack_refused(consumer, "bad", "fallback", target)
            consumer.build.return_value = refused
            artifact.write_bytes(b"incorrect locally rebuilt output")
            with self.assertRaises(e.Failed):
                h.committed_pack_refused(consumer, "bad-artifact", "fallback", target)


    def test_live_jsonl_tolerates_only_an_unfinished_last_append(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "events"
            path.write_bytes(b'{"event":"grant"}\n{"ev')
            self.assertEqual(e.jsonl(path, live=True), [{"event": "grant"}])
            with self.assertRaises(e.Failed):
                e.jsonl(path)
            path.write_bytes(b'bad\n{"ev')
            with self.assertRaises(e.Failed):
                e.jsonl(path, live=True)

    def test_resource_hints_and_json_file_input_without_starting_children(self):
        with tempfile.TemporaryDirectory() as directory:
            run = object.__new__(h.Run)
            run.root = Path(directory)
            run.socket, run.port = Path(directory) / "coordinator.sock", 12345
            base = {"blaze": "/unused", "dist_local": "/unused", "serve": "/unused", "scratch": directory}
            path = Path(directory) / "input.json"
            path.write_text(json.dumps(base))
            run.given = h.load(str(path))
            with patch.dict(os.environ, {"BLAZE_CORES": "24", "BLAZE_DAEMON_MEMORY_GB": "48", "BLAZE_DIST_LOCAL": "fallback"}):
                env = run.env("a", True)
                self.assertEqual(env["BLAZE_CORES"], "24")
                self.assertEqual(env["BLAZE_DAEMON_MEMORY_GB"], "48")
                self.assertNotIn("BLAZE_DIST_LOCAL", env)
                run.given = h.Input(**base, cores=12, daemon_memory_gb=32, env={"BLAZE_CORES": "10"})
                env = run.env("b", True)
                self.assertEqual(env["BLAZE_CORES"], "12")
                self.assertEqual(env["BLAZE_DAEMON_MEMORY_GB"], "32")
            with self.assertRaises(TypeError):
                h.load(json.dumps({**base, "unrecognized": True}))
            with self.assertRaises(e.Failed):
                h.load(json.dumps({**base, "cores": 0}))
            with self.assertRaises(e.Failed):
                h.load(json.dumps({**base, "env": {"BLAZE_DIST_LOCAL": "fallback"}}))
            for field, unsafe in (("program", "../outside"), ("program", "/tmp/not-built"),
                                  ("profile", "debug/../../outside"), ("profile", "")):
                with self.subTest(field=field, unsafe=unsafe), self.assertRaises(e.Failed):
                    h.load(json.dumps({**base, field: unsafe}))

    def test_provenance_rejects_untracked_source_while_preserving_tracked_patch(self):
        with tempfile.TemporaryDirectory() as directory:
            repository = Path(directory) / "repo"
            repository.mkdir()
            def git(*args):
                return subprocess.check_output(["git", "-C", str(repository), *args], stderr=subprocess.PIPE)
            git("init", "-q")
            tracked = repository / "tracked.rs"
            tracked.write_text("original\n")
            git("add", "tracked.rs")
            git("-c", "user.name=fixture", "-c", "user.email=fixture@example.invalid",
                "commit", "-qm", "fixture")
            commit, tree, patch_bytes = h.source_state(repository, 10)
            self.assertEqual(commit, git("rev-parse", "HEAD").decode().strip())
            self.assertEqual(tree, git("rev-parse", "HEAD^{tree}").decode().strip())
            self.assertEqual(patch_bytes, b"")
            untracked = repository / "injected.rs"
            untracked.write_text("untracked compilation input\n")
            with self.assertRaises(e.Failed):
                h.source_state(repository, 10)
            untracked.unlink()
            tracked.write_text("changed\n")
            new_commit, new_tree, patch_bytes = h.source_state(repository, 10)
            self.assertEqual((new_commit, new_tree), (commit, tree))
            self.assertIn(b"changed", patch_bytes)

    def test_artifact_path_cannot_escape_target_even_through_symlink(self):
        with tempfile.TemporaryDirectory() as directory:
            run = object.__new__(h.Run)
            run.root = Path(directory)
            run.project = run.root / "project"
            run.given = h.Input("/unused", "/unused", "/unused", directory)
            executable = run.target("solo") / "debug" / "app"
            executable.parent.mkdir(parents=True)
            executable.write_bytes(b"built output")
            self.assertEqual(run.program("solo"), executable.resolve())
            executable.unlink()
            outside = run.root / "unrelated-program"
            outside.write_bytes(b"not the build output")
            executable.symlink_to(outside)
            with self.assertRaises(e.Failed):
                run.program("solo")

    def test_fault_wait_is_bounded_and_checks_evidence_before_child_exit(self):
        run = object.__new__(h.Run)
        run.root = Path("/unused")
        run.given = h.Input("/unused", "/unused", "/unused", "/unused")
        with patch.object(h.time, "monotonic", side_effect=[0, 0, 2]), patch.object(h.time, "sleep"):
            with self.assertRaises(e.Failed):
                run.until("missing grant", lambda: False, timeout=1)
        dead = unittest.mock.Mock()
        dead.poll.return_value = 0
        self.assertTrue(run.until("already committed", lambda: True, [dead], timeout=1))

    def test_cleanup_resumes_then_terminates_and_bounds_escalation(self):
        child = unittest.mock.Mock(pid=123, stdin=None)
        child.poll.return_value = None
        child.wait.side_effect = [h.subprocess.TimeoutExpired("owned-child", 10), 0]
        with patch.object(h.os, "killpg") as kill:
            h.Run.terminate(child)
        self.assertEqual(kill.call_args_list, [unittest.mock.call(123, h.signal.SIGCONT),
                                              unittest.mock.call(123, h.signal.SIGTERM),
                                              unittest.mock.call(123, h.signal.SIGKILL)])
        self.assertEqual(child.wait.call_args_list, [unittest.mock.call(timeout=10), unittest.mock.call(timeout=10)])


if __name__ == "__main__":
    unittest.main()
