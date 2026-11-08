#!/usr/bin/env python3
"""Small planted-bug controls for the acceptance oracles; starts no Blaze processes."""
from __future__ import annotations

import hashlib
import importlib.util
import json
import os
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


def fixture_cbor(value):
    """Canonical synthetic input for oracle selfchecks only, never acceptance artifacts."""
    def head(major, argument):
        if argument < 24:
            return bytes((major << 5 | argument,))
        size = next(size for size in (1, 2, 4, 8) if argument < 1 << (size * 8))
        return bytes((major << 5 | {1: 24, 2: 25, 4: 26, 8: 27}[size],)) + argument.to_bytes(size, "big")
    if value is None:
        return b"\xf6"
    if type(value) is int:
        return head(0, value)
    if isinstance(value, bytes):
        return head(2, len(value)) + value
    if isinstance(value, str):
        raw = value.encode()
        return head(3, len(raw)) + raw
    if isinstance(value, list):
        return head(4, len(value)) + b"".join(fixture_cbor(item) for item in value)
    if isinstance(value, dict):
        return head(5, len(value)) + b"".join(fixture_cbor(key) + fixture_cbor(item) for key, item in sorted(value.items()))
    raise TypeError(type(value))


def fixture_wire_view(value):
    if isinstance(value, bytes):
        return value.hex()
    if isinstance(value, list):
        return [fixture_wire_view(item) for item in value]
    if isinstance(value, dict):
        return {str(key): fixture_wire_view(item) for key, item in value.items()}
    return value


def fixture_capture(gate, old):
    command = [7, {0: bytes.fromhex(UNIT), 1: old["holder"], 2: {
        0: {0: old["execution"], 1: old["attempt"], 2: old["holder"], 3: old["generation"]},
        1: old["seq"], 2: [{0: bytes.fromhex(name), 1: [0, {0: bytes.fromhex(name)}], 2: [], 3: []} for name in old["work"]],
        3: [], 4: [], 5: []}}]
    raw = fixture_cbor(command)
    (gate / "result.cbor").write_bytes(raw)
    held = {**old, "event": "result_held", "command_sha256": hashlib.sha256(raw).hexdigest()}
    ready = {**held, "command": fixture_wire_view(command)}
    (gate / "ready.json").write_text(json.dumps(ready))
    return ready, [old, held]


def fixture_answer(gate, value):
    raw = fixture_cbor(value)
    (gate / "answer.cbor").write_bytes(raw)
    (gate / "answer.json").write_text(json.dumps({"sha256": hashlib.sha256(raw).hexdigest(), "answer": fixture_wire_view(value)}))


class ResultGateOracles(unittest.TestCase):
    def test_capture_requires_exact_real_grant_carrier_and_product_event(self):
        with tempfile.TemporaryDirectory() as directory:
            gate, old = Path(directory), grant()
            ready, events = fixture_capture(gate, old)
            captured = e.held_result(gate, events, old["pid"])
            self.assertEqual(e.lease(captured), e.lease(old))
            self.assertEqual(captured["capture_sha256"], ready["command_sha256"])
            self.assertIsNone(e.held_result(gate, events[:1], old["pid"]))
            for field, value in (("holder", "other"), ("generation", 30), ("seq", 40), ("attempt", 20), ("execution", 70), ("work", [KEY])):
                with self.assertRaises(e.Failed, msg=field):
                    e.held_result(gate, [{**old, field: value}, events[1]], old["pid"])
            with self.assertRaises(e.Failed):
                e.held_result(gate, events, old["pid"] + 1)
            accepted = {**old, "event": "result_accepted"}
            with self.assertRaises(e.Failed):
                e.held_result(gate, [*events, accepted], old["pid"])
            ready["command"][1]["2"]["0"]["3"] += 1
            (gate / "ready.json").write_text(json.dumps(ready))
            with self.assertRaises(e.Failed):
                e.held_result(gate, events, old["pid"])
            fixture_capture(gate, old)
            (gate / "result.cbor").write_bytes(b"corrupt")
            with self.assertRaises(e.Failed):
                e.held_result(gate, events, old["pid"])

    def test_client_answer_and_unchanged_carrier_are_required_in_both_controls(self):
        with tempfile.TemporaryDirectory() as directory:
            gate, old = Path(directory), grant()
            _, events = fixture_capture(gate, old)
            captured = e.held_result(gate, events, old["pid"])
            self.assertFalse(e.gate_answer(gate, captured, refused=True))
            fixture_answer(gate, [5, {0: [], 1: []}])
            self.assertTrue(e.gate_answer(gate, captured, refused=False))
            with self.assertRaises(e.Failed):
                e.gate_answer(gate, captured, refused=True)
            for token in ("dead-lease", "attempt-moved"):
                fixture_answer(gate, [9, {0: 6, 1: f"{token}: actual typed reason", 2: None}])
                self.assertTrue(e.gate_answer(gate, captured, refused=True))
            for token in ("ended", "holder-lost", "unknown"):
                fixture_answer(gate, [9, {0: 6, 1: f"{token}: wrong reason", 2: None}])
                with self.assertRaises(e.Failed):
                    e.gate_answer(gate, captured, refused=True)
            fixture_answer(gate, [9, {0: 6, 1: "dead-lease: actual typed reason", 2: None}])
            (gate / "answer.cbor").write_bytes(b"corrupt")
            with self.assertRaises(e.Failed):
                e.gate_answer(gate, captured, refused=True)
            fixture_answer(gate, [9, {0: 6, 1: "dead-lease: actual typed reason", 2: None}])
            (gate / "result.cbor").write_bytes(b"changed after capture")
            with self.assertRaises(e.Failed):
                e.gate_answer(gate, captured, refused=True)

    def test_product_gate_failure_is_not_mistaken_for_a_missing_event(self):
        with tempfile.TemporaryDirectory() as directory:
            gate = Path(directory)
            self.assertIsNone(e.held_result(gate, [], 123))
            (gate / "error.txt").write_text("release timed out")
            with self.assertRaises(e.Failed):
                e.held_result(gate, [], 123)
            with self.assertRaises(e.Failed):
                e.gate_answer(gate, grant(), refused=True)

    def test_gate_configuration_is_daemon_only_and_cleanup_releases_before_resume(self):
        with tempfile.TemporaryDirectory() as directory:
            run = object.__new__(h.Run)
            run.root, run.project = Path(directory), Path(directory)
            (run.root / "logs").mkdir()
            run.given = h.Input("/unused", "/unused", "/unused", directory)
            run.daemons, run.result_gates = {}, []
            run.socket, run.port = run.root / "coordinator.sock", 12345
            run.until = unittest.mock.Mock()
            child = unittest.mock.Mock(pid=123)
            child.poll.return_value = None
            gate = run.root / "gate"
            with patch.object(h.subprocess, "Popen", return_value=child) as start:
                run.daemon("a", result_gate=gate)
            self.assertEqual(start.call_args.kwargs["env"]["BLAZE_DIST_TEST_RESULT_GATE"], str(gate))
            self.assertNotIn("BLAZE_DIST_TEST_RESULT_GATE", run.env("a", True))
            with self.assertRaises(e.Failed):
                run.daemon("a", result_gate=run.root / "another")
            run.clients, run.services, run.suspended = [], [], {123}
            def resumed(pid, signal):
                self.assertTrue((gate / "release").is_file(), "cleanup resumed before releasing the gate")
            with patch.object(h.os, "kill", side_effect=resumed), patch.object(h.Run, "terminate"):
                run.stop()
            self.assertTrue((gate / "release").exists())

    def test_a_failed_release_write_still_resumes_and_terminates_owned_children(self):
        run = object.__new__(h.Run)
        run.result_gates, run.suspended = [Path("/unused/gate")], {123}
        run.clients, run.services = [], []
        child = unittest.mock.Mock(pid=123)
        run.daemons, run.socket = {"a": child}, Path("/unused/socket")
        with patch.object(Path, "touch", side_effect=OSError("no space")), patch.object(Path, "unlink"), patch.object(h.os, "kill") as resume, patch.object(h.Run, "terminate") as terminate:
            with self.assertRaises(e.Failed):
                run.stop()
        resume.assert_called_once_with(123, h.signal.SIGCONT)
        terminate.assert_called_once_with(child)


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
        self.assertFalse(e.recovered([old, new, done], old, accepted_after=3))
        self.assertFalse(e.recovered([], old))
        self.assertFalse(e.recovered([old, {**old, "event": "result_accepted"}], old))
        self.assertFalse(e.recovered([old, new, {**done, "work": [KEY]}], old))
        self.assertFalse(e.recovered([old, {**new, "execution": 8}, {**done, "execution": 8}], old))
        self.assertFalse(e.recovered([old, {**new, "unit_digest": OTHER}, done], old))

    def test_refusal_must_name_exact_stale_batch_and_real_reason(self):
        old = grant()
        refusal = {**old, "event": "result_refused", "refusal": "DeadLease"}
        self.assertTrue(e.stale_refused([refusal], old))
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
            manifest.write_text(f"{name} {len(raw)} {OTHER}\n")
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




class ReleaseFoldEvidence(unittest.TestCase):
    def test_actual_kinds_need_two_accepted_holders(self):
        grants = [grant(pid=pid, holder=str(pid), seq=pid, work=[key])
                  for pid, key in ((123, KEY), (124, OTHER))]
        events = [*grants, *[{**g, "event": "result_accepted"} for g in grants]]
        for g in grants:
            events.append({"event": "backend_work", "pid": g["pid"], "unit_digest": UNIT,
                           "work": g["work"], "compiled_keys": g["work"],
                           "release_keys": g["work"], "folded_keys": g["work"]})
        rows = [row(compiled_for_work=2, work_compiled_keys=[KEY, OTHER])]
        e.release_fold(rows, events)
        for plant in ("one_holder", "no_accept", "planned_only", "unreported"):
            changed = json.loads(json.dumps(events))
            if plant == "one_holder":
                changed = [event for event in changed if event.get("pid") != 124 and event.get("holder") != "124"]
            elif plant == "no_accept":
                changed = [event for event in changed if event["event"] != "result_accepted"]
            elif plant == "planned_only":
                changed[-1]["compiled_keys"] = []
            else:
                changed[-1]["compiled_keys"].append("ff" * 32)
            with self.subTest(plant=plant), self.assertRaises(e.Failed):
                e.release_fold(rows, changed)

if __name__ == "__main__":
    unittest.main()
