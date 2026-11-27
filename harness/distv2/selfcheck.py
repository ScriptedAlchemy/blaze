#!/usr/bin/env python3
"""Small planted-bug controls for the acceptance oracles; starts no Blaze processes."""
from __future__ import annotations

import hashlib
import importlib.util
import json
import os
import struct
import subprocess
from concurrent.futures import ThreadPoolExecutor
from http.client import HTTPConnection
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from blake3 import blake3
import sys
import tempfile
import time
from threading import Thread
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
            "attempt": 2, "publication_attempt": 2, "joined": True, "grants": 1, "compiled_for_work": 1,
            "tail_compiled": 0, "stored_for_work": 0, "work_compiled_keys": [KEY],
            "tail_compiled_keys": [], **extra}


def grant(**extra):
    return {"event": "grant", "pid": 123, "unit_digest": UNIT, "execution": 7, "attempt": 2,
            "holder": "a", "generation": 3, "seq": 4, "work": [KEY, OTHER], **extra}


def write_pack(blobs, values):
    raw = b"blzdpk\0\x01" + b"".join(bytes.fromhex(key) + bytes((kind,))
        + len(value).to_bytes(8, "little") + value for key, kind, value in values)
    name = hashlib.sha256(raw).hexdigest()
    (blobs / "packs" / f"{name}.pack").write_bytes(raw)
    return name, len(raw)


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

    def test_only_the_planned_manifest_waits_for_release(self):
        selected = f"/v1/list/manifests/{UNIT}"
        other = f"/v1/list/manifests/{OTHER}"

        class Origin(BaseHTTPRequestHandler):
            def do_GET(self):
                self.send_response(404)
                self.send_header("Content-Length", "0")
                self.end_headers()

        with tempfile.TemporaryDirectory() as tmp, ThreadingHTTPServer(("127.0.0.1", 0), Origin) as origin:
            worker = Thread(target=origin.serve_forever, kwargs={"poll_interval": 0.05}, daemon=True)
            worker.start()
            try:
                given = h.Input(blaze="blaze", dist_local="local", serve="serve", scratch=tmp, command_timeout=3)
                with patch.dict(os.environ, HOME=tmp):
                    run = h.Run(given)
                run.port = origin.server_port
                with run.hold_manifest_lookup("a", "app") as (proxy, requested, release), ThreadPoolExecutor(1) as reader:
                    Path(f"{run.report_path('a')}.plans").write_text(json.dumps([{"unit": "app", "unit_digest": UNIT}]) + "\n")

                    def get(path):
                        client = HTTPConnection(proxy, timeout=3)
                        try:
                            client.request("GET", path, headers={"Content-Length": "0", "Connection": "close"})
                            return client.getresponse().status
                        finally:
                            client.close()

                    self.assertEqual(get(other), 404)
                    self.assertFalse(requested.is_set())
                    held = reader.submit(get, selected)
                    self.assertTrue(requested.wait(2))
                    self.assertFalse(held.done())
                    release.set()
                    self.assertEqual(held.result(timeout=2), 404)
            finally:
                origin.shutdown()
                worker.join(timeout=3)

    def test_pack_proxy_holds_the_body_and_not_the_earlier_miss(self):
        missing = f"/v1/list/manifests/{UNIT}"
        pack = f"/v1/blob/packs/{KEY}.pack"
        payload = b"native pack response"

        class Origin(BaseHTTPRequestHandler):
            def do_GET(self):
                body = payload if self.path == pack else b""
                self.send_response(200 if body else 404)
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)

        with tempfile.TemporaryDirectory() as tmp, ThreadingHTTPServer(("127.0.0.1", 0), Origin) as origin:
            worker = Thread(target=origin.serve_forever, kwargs={"poll_interval": 0.05}, daemon=True)
            worker.start()
            try:
                given = h.Input(blaze="blaze", dist_local="local", serve="serve", scratch=tmp, command_timeout=3)
                with patch.dict(os.environ, HOME=tmp):
                    run = h.Run(given)
                run.port = origin.server_port
                file = run.root / "cached.pack"
                file.write_bytes(payload)
                held = h.HeldPack(UNIT, {KEY: file}, 3, speculative_miss=True, event_mark=lambda: 23)
                with run.cache_proxy("pack-selfcheck", before_reply=held.before_reply, before_body=held.before_body) as proxy:
                    def get(path):
                        client = HTTPConnection(proxy, timeout=3)
                        try:
                            client.request("GET", path, headers={"Content-Length": "0"})
                            response = client.getresponse()
                            return response.status, response.read()
                        finally:
                            client.close()

                    with ThreadPoolExecutor(2) as reader:
                        try:
                            self.assertEqual(get(missing), (404, b""))
                            self.assertFalse(held.held.is_set())
                            demand = reader.submit(get, missing)
                            body = reader.submit(get, pack)
                            self.assertTrue(held.held.wait(1))
                            self.assertEqual(demand.result(timeout=1), (404, b""))
                            self.assertFalse(body.done())
                            receipt, misses = held.observed()
                            self.assertEqual(receipt["event_mark"], 23)
                            self.assertEqual([miss["status"] for miss in misses], [404, 404])
                        finally:
                            held.release.set()
                        self.assertEqual(body.result(timeout=1), (200, payload))
                # With no speculative transfer, the serial compiler still sees its real miss.
                serial = h.HeldPack(UNIT, {KEY: file}, 0, speculative_miss=False, event_mark=lambda: 0)
                self.assertTrue(serial.before_reply(missing, 404, 0))
                self.assertTrue(serial.miss_before_pack)
                with self.assertRaises(e.Failed):
                    serial.observed()
            finally:
                origin.shutdown()
                worker.join(timeout=3)

    def test_overlap_needs_a_compiled_batch_before_successful_publication(self):
        with tempfile.TemporaryDirectory() as tmp:
            blobs = Path(tmp)
            (blobs / "packs").mkdir()
            (blobs / "manifests").mkdir()
            name, size = write_pack(blobs, [(KEY, 0, b"compiler record")])
            (blobs / "manifests" / UNIT).write_text(f"{name} {size} {e.pack_keys_digest({KEY})}\n")
            ready = blobs / "ready.json"
            ready.write_text(json.dumps({"command": [7, {"2": {"3": [{"0": name}]}}]}))
            issued = grant(work=[KEY])
            accepted = {**issued, "event": "result_accepted"}
            published = {"event": "published", "unit_digest": UNIT, "execution": 7, "attempt": 2, "packs": [name]}
            done = {"event": "done", "unit_digest": UNIT, "execution": 7, "attempt": 2, "outcome": "Passed"}
            events = [issued, accepted, published, done]
            e.overlap_publication(blobs, blobs, row(), issued, events)
            for plant in ([accepted, issued, published, done], [issued, published, accepted, done],
                          [issued, accepted, published], [issued, {**accepted, "work": []}, published, done],
                          [issued, {**accepted, "work": [OTHER]}, published, done]):
                with self.subTest(plant=plant), self.assertRaises(e.Failed):
                    e.overlap_publication(blobs, blobs, row(), issued, plant)
            with self.assertRaisesRegex(e.Failed, "contains no record compiled"):
                e.overlap_publication(blobs, blobs, row(compiled_for_work=0, work_compiled_keys=[]), issued, events)
            other, _ = write_pack(blobs, [(OTHER, 0, b"unrelated record")])
            ready.write_text(json.dumps({"command": [7, {"2": {"3": [{"0": other}]}}]}))
            with self.assertRaisesRegex(e.Failed, "contains no record compiled"):
                e.overlap_publication(blobs, blobs, row(), issued, events)

    def test_peer_must_commit_disjoint_work_in_the_held_execution_and_attempt(self):
        held = grant(work=[KEY])
        other = grant(pid=124, holder="b", generation=4, work=[OTHER])
        accepted = {**other, "event": "result_accepted"}
        self.assertTrue(e.peer_result_before_release([held, other, accepted], held, 124))
        self.assertFalse(e.peer_result_before_release([held, other], held, 124))
        self.assertFalse(e.peer_result_before_release([held, other, accepted], held, 123))
        for field, value in (("execution", 8), ("attempt", 3), ("unit_digest", KEY), ("holder", "a")):
            changed = {**other, field: value}
            self.assertFalse(e.peer_result_before_release([held, changed, {**changed, "event": "result_accepted"}], held, 124))
        with self.assertRaisesRegex(e.Failed, "overlapping work"):
            e.peer_result_before_release([held, {**other, "work": [KEY]}, {**accepted, "work": [KEY]}], held, 124)
        with self.assertRaisesRegex(e.Failed, "before its peer"):
            e.peer_result_before_release([held, {**held, "event": "result_accepted"}, other, accepted], held, 124)

    def test_capture_requires_exact_real_grant_carrier_and_product_event(self):
        with tempfile.TemporaryDirectory() as directory:
            gate, old = Path(directory), grant()
            ready, events = fixture_capture(gate, old)
            captured = e.held_result(gate, events, old["pid"])
            self.assertEqual(e.lease(captured), e.lease(old))
            self.assertEqual(captured["capture_sha256"], ready["command_sha256"])
            self.assertIsNone(e.held_result(gate, events[:1], old["pid"]))
            self.assertIsNone(e.held_result(gate, [], old["pid"]))
            with self.assertRaisesRegex(e.Failed, "no matching actual issued grant"):
                e.held_result(gate, events[1:], old["pid"])
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

    def test_gate_is_passed_to_its_export_process_and_cleanup_releases_before_resume(self):
        with tempfile.TemporaryDirectory() as directory:
            with patch.object(h, "free_port", return_value=12345), patch.dict(os.environ, {"HOME": directory}):
                run = h.Run(h.Input("/unused", "/unused", "/unused", directory))
            run.project = run.root / "project"
            run.socket = run.root / "coordinator.sock"
            child = unittest.mock.Mock(pid=123)
            gate = run.root / "gate"
            with patch.object(h.subprocess, "Popen", return_value=child) as start:
                process, _ = run.build("a", wait=False, result_gate=gate, result_unit="mathy")
                run.build("b", wait=False, result_gate=gate, result_unit="mathy")
            self.assertEqual(run.result_gates, [gate])
            self.assertFalse((gate / "keepalive").exists())
            self.assertIs(process, child)
            self.assertEqual(start.call_args.args[0][:2], ["/unused", "export"])
            self.assertTrue(start.call_args.kwargs["start_new_session"])
            self.assertEqual(start.call_args.kwargs["env"]["BLAZE_DIST_TEST_RESULT_GATE"], str(gate.resolve()))
            self.assertEqual(start.call_args.kwargs["env"]["BLAZE_DIST_TEST_RESULT_UNIT"], "mathy")
            self.assertNotIn("BLAZE_DIST_TEST_RESULT_GATE", run.env("a", True))
            with patch.object(h.evidence, "held_result", return_value={"pid": 123}) as held, \
                 patch.object(h.evidence, "accepted", return_value=False), \
                 patch.object(run, "events", return_value=[]):
                self.assertEqual(run.held_result("a", child, gate, 0, suspend=False), {"pid": 123})
            held.assert_called_once_with(gate, [], child.pid)
            run.suspended.add(child.pid)
            def resumed(pid, signal):
                self.assertTrue((gate / "release").is_file(), "cleanup resumed before releasing the gate")
            with patch.object(h.os, "kill", side_effect=resumed), patch.object(h.Run, "terminate"):
                run.stop()
            self.assertTrue((gate / "release").exists())

    def test_fault_pair_uses_one_keepalive_gate_and_selects_the_peer_of_the_actual_lead(self):
        with tempfile.TemporaryDirectory() as directory:
            with patch.object(h, "free_port", return_value=12345), patch.dict(os.environ, {"HOME": directory}):
                run = h.Run(h.Input("/unused", "/unused", "/unused", directory))
            run.project = run.root / "project"
            run.socket = run.root / "coordinator.sock"
            a, b = unittest.mock.Mock(pid=123), unittest.mock.Mock(pid=124)
            gate = run.root / "fault"
            captured = {"pid": b.pid}
            with patch.object(h.subprocess, "Popen", side_effect=[a, b]) as start, \
                 patch.object(run, "peer_result_while_held", return_value=captured) as peer:
                held, victim, survivor = run.fault_pair(gate, 7)
            peer.assert_called_once_with(gate, a, b, 7)
            self.assertEqual((held, victim[:2], survivor[:2]), (captured, ("b", b), ("a", a)))
            self.assertTrue((gate / "keepalive").is_file())
            self.assertEqual(run.result_gates, [gate])
            for call in start.call_args_list:
                self.assertEqual(call.kwargs["env"]["BLAZE_DIST_TEST_RESULT_GATE"], str(gate.resolve()))

    def test_a_failed_release_write_still_resumes_and_terminates_owned_children(self):
        run = object.__new__(h.Run)
        run.result_gates, run.suspended = [Path("/unused/gate")], {123}
        child = unittest.mock.Mock(pid=123)
        run.clients, run.services = [(child, 0, "/unused/log")], []
        run.socket = Path("/unused/socket")
        with patch.object(Path, "touch", side_effect=OSError("no space")), patch.object(Path, "unlink"), patch.object(h.os, "kill") as resume, patch.object(h.Run, "terminate") as terminate:
            with self.assertRaises(e.Failed):
                run.stop()
        resume.assert_called_once_with(123, h.signal.SIGCONT)
        terminate.assert_called_once_with(child)


class Oracles(unittest.TestCase):
    def test_release_reports_reached_native_crates(self):
        native = [row(unit=name) for name in ("shapes", "mathy", "app")]
        with_macro = [*native, row(unit="pm")]
        e.fixture_units("a", native, release=True)
        e.fixture_units("b", with_macro, release=False)
        with self.assertRaisesRegex(e.Failed, "omitted fixture units: mathy"):
            e.fixture_units("a", [native[0], native[2]], release=True)
        with self.assertRaisesRegex(e.Failed, "native release unit for the Wasm proc macro pm"):
            e.fixture_units("a", with_macro, release=True)
        with self.assertRaisesRegex(e.Failed, "omitted fixture units: pm"):
            e.fixture_units("b", native, release=False)

    def test_unit_plan_waits_for_the_end_of_the_snapshot(self):
        with tempfile.TemporaryDirectory() as temp:
            path = Path(temp) / "worker.plans"
            self.assertIsNone(e.unit_plan(path))
            path.write_bytes(b'[{"unit":"mathy"}]')
            self.assertIsNone(e.unit_plan(path))
            path.write_bytes(b'[{"unit":"mathy"}]\n')
            self.assertEqual(e.unit_plan(path), [{"unit": "mathy"}])
            path.write_bytes(b'{}\n')
            with self.assertRaisesRegex(e.Failed, "expected a list of unit plans"):
                e.unit_plan(path)

    def test_equal_source_rejects_split_plans(self):
        planned = row(plan_names=[KEY, OTHER], plan_inputs=[OTHER, KEY])
        e.same_unit_plans([planned], [dict(planned)])
        for field, value in (("unit_digest", OTHER), ("plan_names", [OTHER, KEY]), ("plan_inputs", [KEY, OTHER])):
            with self.assertRaisesRegex(e.Failed, f"same source: mathy \\({field}\\)"):
                e.same_unit_plans([planned], [{**planned, field: value}])

    def test_final_export_cannot_hide_emission_outside_the_worker_reports(self):
        clean = "export stats: compiled=0 cached=20 unkeyed=0 lto=0 inlined=0"
        e.no_local_emission("cold", clean)
        for name in ("compiled", "unkeyed", "lto"):
            with self.subTest(name=name), self.assertRaisesRegex(e.Failed, f"requires {name}=0"):
                e.no_local_emission("cold", clean.replace(f"{name}=0", f"{name}=1"))
        for invalid in ("", clean + "\n" + clean, clean + " lto=0"):
            with self.assertRaises(e.Failed):
                e.no_local_emission("cold", invalid)

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
            done = {"event": "done", "unit_digest": UNIT, "execution": 7, "attempt": 2, "outcome": "Passed"}
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

    def test_publication_keeps_accepting_attempt_and_compares_logical_pack_values(self):
        with tempfile.TemporaryDirectory() as directory:
            blobs = Path(directory)
            (blobs / "packs").mkdir()
            (blobs / "manifests").mkdir()

            a = (KEY, 1, b"x")
            b = (OTHER, 2, b"y")
            merged, size = write_pack(blobs, [a, b])
            left, _ = write_pack(blobs, [a])
            right, _ = write_pack(blobs, [b])
            (blobs / "manifests" / UNIT).write_text(f"{merged} {size} {e.pack_keys_digest({KEY, OTHER})}\n")
            published = {"event": "published", "unit_digest": UNIT, "execution": 7,
                         "attempt": 1, "packs": [left, right]}
            done = {"event": "done", "unit_digest": UNIT, "execution": 7, "attempt": 2, "outcome": "Passed"}
            report = row(publication_attempt=1)
            e.publication(blobs, [report], [published, done])
            e.publication(blobs, [report], [{**done, "unit_digest": OTHER}, published, done])
            # Same keys with different bytes or kind cannot masquerade as another partition.
            for bad_values in ([a, (OTHER, 2, b"changed")], [a, (OTHER, 3, b"y")], [a]):
                bad, _ = write_pack(blobs, bad_values)
                with self.assertRaises(e.Failed):
                    e.publication(blobs, [report], [{**published, "packs": [bad]}, done])
            for bad_events in ([done, published], [{**published, "attempt": 2}, done],
                               [published, {**done, "attempt": 1}]):
                with self.assertRaises(e.Failed):
                    e.publication(blobs, [report], bad_events)
            with self.assertRaises(e.Failed):
                e.publication(blobs, [row(publication_attempt=3)], [published, done])

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
                self.assertNotIn("BLAZE_DAEMON_MEMORY_GB", env)
                self.assertNotIn("BLAZE_DIST_LOCAL", env)
                run.given = h.Input(**base, cores=12, daemon_memory_gb=32, env={"BLAZE_CORES": "10"})
                env = run.env("b", True)
                self.assertEqual(env["BLAZE_CORES"], "12")
                self.assertNotIn("BLAZE_DAEMON_MEMORY_GB", env)
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
            for field, invalid in (("faults", 0), ("faults", 1), ("faults", "false"), ("faults", None),
                                   ("release_fold", 0), ("release_fold", "true"),
                                   ("args", "--release"), ("args", [1]), ("args", ["bad\0argument"]),
                                   ("run", "11"), ("run", [False]), ("run", None),
                                   ("timeout", True), ("command_timeout", 1.5), ("cores", True),
                                   ("daemon_memory_gb", "6"), ("stall_bound", False), ("stall_bound", 0),
                                   ("blaze", ""), ("dist_local", ["/unused"]), ("serve", "bad\0path"),
                                   ("scratch", False), ("project", None)):
                with self.subTest(field=field, invalid=invalid), self.assertRaisesRegex(e.Failed, field):
                    h.load(json.dumps({**base, field: invalid}))
            with self.assertRaisesRegex(e.Failed, "JSON object"):
                h.load("[]")
            self.assertTrue(h.load(json.dumps(base)).faults)
            self.assertFalse(h.load(json.dumps({**base, "faults": False})).faults)
            release = h.load(json.dumps({**base, "faults": True, "release_fold": True,
                                         "args": ["--release"], "profile": "release", "run": ["11"],
                                         "stall_bound": 90}))
            self.assertTrue(release.faults and release.release_fold)
            self.assertEqual(release.args, ("--release",))
            self.assertEqual(release.run, ("11",))
            self.assertEqual(release.stall_bound, 90)

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
        evidence = unittest.mock.Mock(side_effect=[False, True])
        self.assertTrue(run.until("committed before exit", evidence, [dead], timeout=1))

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
    def setUp(self):
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        self.blobs = Path(directory.name)
        (self.blobs / "packs").mkdir()
        (self.blobs / "manifests").mkdir()
        self.values = {}
        self.events = []
        self.rows = []
        # All these distinct templates genuinely share the same minimal 'ret' machine record.
        machine = b"bzcode09" + bytes(4) + bytes((4, 3, 0, 0, 4)) + bytes(47) + bytes.fromhex("c0035fd6") + bytes(4)
        content_domain = "blaze-ladder code entry content 1"
        content = blake3(machine, derive_key_context=content_domain).digest()
        self.values[content.hex()] = (0, machine)

        def wire(recipe):
            cbor = fixture_cbor([0, {0: bytes.fromhex(UNIT), 1: recipe, 2: bytes(32)}])
            return hashlib.sha256(b"blaze/WorkName/v5\0" + cbor).hexdigest()

        for pid in (123, 124):
            def item(n):
                return bytes((pid, n)) * 16
            body = item(3)
            members = [(item(4), item(5)), (item(6), item(7))]
            group = blake3(body + (2).to_bytes(8, "little") + b"".join(s + r for s, r in members),
                           derive_key_context="blaze-ladder folded grant 1").digest()
            release, fold = wire(item(1)), wire(group)
            native = []
            for work, role, recipe, symbol in [(release, 1, item(1), item(2)), (fold, 2, body, body),
                                                *((fold, 3, r, s) for s, r in members)]:
                receipt = b"bznout01" + bytes((role,)) + recipe + symbol + content
                key = blake3(receipt, derive_key_context=content_domain).digest()
                self.values[key.hex()] = (0, receipt)
                binding = blake3(recipe, derive_key_context="blaze-ladder backend output binding 2").hexdigest()
                self.values[binding] = (0, key)
                native.append([work, key.hex(), content.hex()])
            g = grant(pid=pid, holder=str(pid), seq=pid, work=[release, fold])
            made = [key for _, key, _ in native]
            self.events.extend([g, {**g, "event": "result_accepted"}, {**g, "event": "backend_work",
                "compiled_keys": made, "release_keys": made[:1], "folded_keys": made[1:], "native_outputs": native}])
            self.rows.append(row(compiled_for_work=len(made), work_compiled_keys=made))
        self.publish(self.values)

    def publish(self, values):
        pack, size = write_pack(self.blobs, [(k, *values[k]) for k in sorted(values)])
        (self.blobs / "manifests" / UNIT).write_text(f"{pack} {size} {e.pack_keys_digest(set(values))}\n")

    def test_actual_kinds_need_two_accepted_holders(self):
        # Shared with the native grant_events test, which obtains this name via WorkName::of.
        self.assertEqual(e.wire_work("01" * 32, bytes((3,)) * 32),
                         "96f7df1cb0f7249afc2e67903fac12e9b9156571d2b685c84e1472411a58c4bb")
        e.release_fold(self.blobs, self.rows, self.events)
        for plant in ("one_holder", "no_accept", "planned_only", "unreported", "same_key",
                      "wrong_execution", "expired_lease", "other_batch", "different_result",
                      "swapped_grants", "missing_receipt", "duplicate_credit", "wrong_content", "incomplete_fold"):
            changed = json.loads(json.dumps(self.events))
            if plant == "one_holder":
                changed = [event for event in changed if event.get("pid") != 124 and event.get("holder") != "124"]
            elif plant == "no_accept":
                changed = [event for event in changed if event["event"] != "result_accepted"]
            elif plant == "planned_only":
                changed[-1]["compiled_keys"] = []
            elif plant == "unreported":
                changed[-1]["compiled_keys"].append("ff" * 32)
            elif plant == "same_key":
                changed[-1]["folded_keys"] = changed[-1]["release_keys"]
            elif plant == "different_result":
                next(ev for ev in changed if ev["event"] == "result_accepted")["work"] = []
            elif plant == "swapped_grants":
                outputs = changed[-1]["native_outputs"]
                outputs[0][0], outputs[1][0] = outputs[1][0], outputs[0][0]
            elif plant == "missing_receipt":
                changed[-1]["native_outputs"].pop()
            elif plant == "duplicate_credit":
                changed.append(changed[-1])
            elif plant == "wrong_content":
                changed[-1]["native_outputs"][0][2] = KEY
            elif plant == "incomplete_fold":
                for field in ("native_outputs", "folded_keys", "compiled_keys"):
                    changed[-1][field].pop()
            else:
                changed[-1][{"wrong_execution": "execution", "expired_lease": "generation", "other_batch": "seq"}[plant]] += 1
            with self.subTest(plant=plant), self.assertRaises(e.Failed):
                e.release_fold(self.blobs, self.rows, changed)

    def test_rehashes_accepted_receipt_binding_and_machine_bytes(self):
        _, receipt, content = self.events[-1]["native_outputs"][0]
        raw = self.values[receipt][1]
        binding = blake3(raw[9:41], derive_key_context="blaze-ladder backend output binding 2").hexdigest()
        for key, replaced in ((receipt, None), (receipt, (2, raw)), (receipt, (0, raw[:8] + b"\x03" + raw[9:])),
                              (binding, (0, bytes.fromhex(content))), (content, (0, self.values[content][1][:-1] + b"x"))):
            values = self.values.copy()
            if replaced is None:
                del values[key]
            else:
                values[key] = replaced
            self.publish(values)
            with self.subTest(key=key), self.assertRaises(e.Failed):
                e.release_fold(self.blobs, self.rows, self.events)

    def test_machine_extent_includes_cfi_and_checks_its_padding(self):
        raw = next(value for _, value in self.values.values() if value.startswith(b"bzcode09"))
        cfi = b"\x04\0\0\0\0\0\0\0\x01\xf8\0\0"
        raw = b"bzcode10" + raw[8:60] + len(cfi).to_bytes(4, "little") + raw[64:] + cfi + bytes(4)
        self.assertTrue(e.machine_record(raw))
        self.assertFalse(e.machine_record(raw[:-1] + b"\x01"))
        self.assertFalse(e.machine_record(raw[:-8]))

HOST_UNIT = "00" * 32
HOST_JOB = "f57ea1a0751c3bd3416494c4e4b2912dfdefa542b90d2b95d2301f99be3bb016"
HOST_KEYS = [f"{index + 1:02x}" * 32 for index in range(3)]
HOST_ARTIFACT = "ff" * 32


def host_test_held(index: int) -> dict:
    grant = {"event": "grant", "pid": 1, "unit_digest": HOST_UNIT, "execution": 9, "attempt": 0,
             "holder": f"holder-{index}", "generation": 1, "seq": 0, "work": [HOST_KEYS[index]],
             "capture_sha256": "aa" * 32}
    return {"grant": grant, "capture": {"command_sha256": grant["capture_sha256"],
                                       "command": [7, {"0": HOST_JOB}]}}


class HostProof(unittest.TestCase):
    def test_benchmark_sessions_pin_before_preparing_or_spawning_children(self):
        with tempfile.TemporaryDirectory() as directory:
            node = {"checkout": directory, "scratch": str(Path(directory) / "session"), "blaze": "/blaze",
                    "python": sys.executable, "runner": "runner", "socket": "/coordinator", "blobs": "127.0.0.1:80"}
            worker = h.HostWorker(**node, role="helper", namespace="sample", cores=2, timeout=60, benchmark=True)
            controller = h.HostController(**node, role="controller", namespace="sample", cores=2, timeout=60,
                                          benchmark=True, dist_local="/coord", serve="/serve", ssh_config="/config",
                                          helpers=[h.HostHelper(**node, alias=role) for role in ("helper-1", "helper-2")])
            for start, request in ((h.host_worker, worker), (h.host_controller, controller)):
                with (self.subTest(role=request.role),
                      patch.object(h, "host_pin_cores", side_effect=e.Failed("pin refused")) as pin,
                      self.assertRaisesRegex(e.Failed, "pin refused")):
                    start(request)
                pin.assert_called_once_with(2)
                self.assertFalse(Path(node["scratch"]).exists())

    @unittest.skipUnless(sys.platform.startswith("linux"), "the benchmark requires Linux affinity")
    def test_benchmark_affinity_is_inherited_through_child_and_grandchild_exec(self):
        # Only the kernel can prove the production launcher preserves the CPU mask
        # through exec and a new session. A Python probe stands in for the compiler;
        # never narrow the selfcheck runner itself or start Blaze during a selfcheck.
        available = sorted(os.sched_getaffinity(0))
        cores = min(2, len(available))
        code = """
import importlib.util, json, os, subprocess, sys
from pathlib import Path
sys.path.insert(0, sys.argv[1])
spec = importlib.util.spec_from_file_location("distv2_harness", Path(sys.argv[1]) / "distv2.py")
h = importlib.util.module_from_spec(spec)
sys.modules[spec.name] = h
spec.loader.exec_module(h)
cores = int(sys.argv[2])
h.host_pin_cores(cores)
root = Path(sys.argv[3])
given = h.HostWorker(checkout=str(root), scratch=str(root), blaze=str(root / "probe"),
    python=sys.executable, runner="probe", socket="/unused", blobs="127.0.0.1:80", role="helper",
    namespace="selfcheck", cores=cores, timeout=10, benchmark=True)
with (root / "launch.log").open("xb") as log:
    process = h.host_spawn_export(given, root, root, os.environ.copy(), log)
try:
    if process.wait(timeout=10) != 0:
        raise RuntimeError((root / "launch.log").read_text())
finally:
    h.host_stop(process)
launched = json.loads((root / "launch.log").read_text())
try:
    h.host_pin_cores(cores + 1)
except h.Failed as error:
    refused = str(error)
else:
    refused = None
print(json.dumps({**launched, "after": h.host_affinity(), "refused": refused}))
"""
        with tempfile.TemporaryDirectory() as directory:
            probe = Path(directory) / "probe"
            probe.write_text(f"#!{sys.executable}\n"
                "import json, os, subprocess, sys\n"
                "grandchild = json.loads(subprocess.check_output([sys.executable, '-c', "
                "'import json, os; print(json.dumps(sorted(os.sched_getaffinity(0))))'], "
                "text=True, timeout=5, start_new_session=True))\n"
                "print(json.dumps({'child': sorted(os.sched_getaffinity(0)), 'grandchild': grandchild}))\n")
            probe.chmod(0o755)
            output = subprocess.check_output([sys.executable, "-c", code, str(HARNESS), str(cores), directory],
                                             text=True, timeout=20, start_new_session=True)
        observed = json.loads(output)
        for name in ("child", "grandchild", "after"):
            self.assertEqual(observed[name], available[:cores])
        self.assertIn(f"needs {cores + 1} CPUs", observed["refused"])
        self.assertEqual(sorted(os.sched_getaffinity(0)), available)

    def test_benchmark_refuses_missing_or_ineffective_affinity(self):
        with patch.object(h.os, "sched_getaffinity", None, create=True), self.assertRaisesRegex(e.Failed, "requires Linux CPU affinity"):
            h.host_pin_cores(2)
        with (patch.object(h, "host_affinity", return_value=[3, 5, 7]),
              patch.object(h.os, "sched_setaffinity", create=True),
              self.assertRaisesRegex(e.Failed, "kernel did not retain")):
            h.host_pin_cores(2)

    def test_benchmark_checks_ancestor_quotas_and_effective_cpuset(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            proc = root / "proc"
            proc.mkdir()
            mount = root / "cgroup mount"
            leaf = mount / "parent" / "leaf"
            leaf.mkdir(parents=True)
            (proc / "cgroup").write_text("0::/parent/leaf\n")
            mounted = str(mount).replace(" ", r"\040")
            (proc / "mountinfo").write_text(f"1 2 0:3 / {mounted} rw - cgroup2 cgroup rw\n")
            (mount / "cgroup.controllers").write_text("cpuset cpu io\n")
            (mount / "cpuset.cpus.effective").write_text("0-7\n")
            (leaf.parent / "cpuset.cpus.effective").write_text("3,5-6\n")
            (leaf.parent / "cpu.max").write_text("200000 100000\n")
            (leaf / "cpu.max").write_text("max 100000\n")
            result = h.host_cgroup_budget(2, [3, 5], proc=proc)
            self.assertEqual(result["effective_cpuset"], [3, 5, 6])
            self.assertEqual(result["effective_cpu_max"],
                             {"path": str(leaf.parent), "quota_us": 200000, "period_us": 100000})
            self.assertEqual([row["cpu.max"] for row in result["limits"]],
                             ["max 100000", "200000 100000", None])
            # A leaf marked unlimited still inherits the parent's sub-two-core cap.
            (leaf.parent / "cpu.max").write_text("150000 100000\n")
            with self.assertRaisesRegex(e.Failed, "allows less than 2 benchmark cores"):
                h.host_cgroup_budget(2, [3, 5], proc=proc)
            (leaf.parent / "cpu.max").write_text("max 100000\n")
            (leaf.parent / "cpuset.cpus.effective").write_text("3\n")
            with self.assertRaisesRegex(e.Failed, "escapes effective cgroup cpuset"):
                h.host_cgroup_budget(2, [3, 5], proc=proc)
            (proc / "mountinfo").write_text("")
            with self.assertRaisesRegex(e.Failed, "full cgroup v2 hierarchy"):
                h.host_cgroup_budget(2, [3, 5], proc=proc)

    def test_benchmark_requires_the_reported_body_pool_to_match_the_budget(self):
        with tempfile.TemporaryDirectory() as directory:
            profile = Path(directory) / "profile.txt"
            row = "Bodies floor: largest task 1.0 ms `leaf_256`; at 2 workers the same sum would be 1.0 ms\n"
            profile.write_text(row)
            self.assertEqual(h.host_body_workers(profile, 2), 2)
            for changed in (row.replace("at 2 workers", "at 4 workers"), row + row, ""):
                profile.write_text(changed)
                with self.subTest(profile=changed), self.assertRaises(e.Failed):
                    h.host_body_workers(profile, 2)

    def test_benchmark_keeps_primary_latency_and_rejects_a_stale_helper_artifact(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            node = {"checkout": directory, "scratch": directory, "blaze": "/blaze", "python": sys.executable,
                    "runner": "runner", "socket": "/coordinator", "blobs": "127.0.0.1:80"}
            request = h.load_host(dict(**node, role="controller", namespace="sample", cores=2, timeout=60,
                                       benchmark=True, solo_first=False, dist_local="/coord", serve="/serve",
                                       ssh_config="/config", helpers=[dict(alias=role, **node) for role in ("helper-1", "helper-2")]))
            cold_source, cold_stdout = h.host_fixture(root / "project")
            edit_source, edit_stdout = h.host_edit_fixture(root / "project")
            identity = {"runner": "primary", "observed_body_workers": 2,
                        "resources": {"configured_workers": 2, "sched_affinity": [3, 5]}}
            solo = {phase: {"source_sha256": source, "artifact": {"stdout": stdout, "sha256": source},
                            "elapsed_ns": 100, "identity": identity}
                    for phase, source, stdout in (("cold", cold_source, cold_stdout), ("edit", edit_source, edit_stdout))}
            h.host_check_latency_edit(solo["cold"], solo["edit"])
            distributed = {phase: [dict(row, elapsed_ns=elapsed) for elapsed in (75, 40, 95)]
                           for phase, row in solo.items()}
            h.host_latency_report(root, request, [{"runner": name} for name in ("primary", "helper-1", "helper-2")],
                                  solo, distributed)
            report = json.loads((root / "three-host-latency.json").read_text())
            self.assertEqual(report["schema"], 2)
            self.assertEqual(report["phases"]["edit"]["primary_export_ns"], 75)
            self.assertEqual(report["order"], ["distributed", "solo"])
            distributed["edit"][2]["identity"] = {**identity, "observed_body_workers": 4}
            with self.assertRaisesRegex(e.Failed, "workers must match the core budget"):
                h.host_latency_report(root, request, list(report["hosts"].values()), solo, distributed)
            distributed["edit"][2]["identity"] = identity
            distributed["edit"][2]["artifact"] = solo["cold"]["artifact"]
            with self.assertRaisesRegex(e.Failed, "must match the solo control"):
                h.host_latency_report(root, request, list(report["hosts"].values()), solo, distributed)

    def test_host_helpers_enable_the_result_gate_for_all_holders(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            request = h.HostWorker(
                checkout=directory, scratch=str(root / "helper"), blaze="/blaze", python=sys.executable,
                runner="helper", socket="/coordinator", blobs="127.0.0.1:80", role="helper",
                namespace="selfcheck", cores=1, timeout=1,
            )
            with (
                patch.object(h, "host_worker_env", return_value={}),
                patch.object(h, "host_node_identity", return_value={}),
                patch.object(h, "HostMessages"),
                patch.object(h, "host_send"),
                patch.object(h, "host_pin_cores") as pin,
                patch.object(h, "host_run_export", side_effect=e.Failed("capture launch")) as launch,
                self.assertRaisesRegex(e.Failed, "capture launch"),
            ):
                h.host_worker(request)
            pin.assert_not_called()
            environment = launch.call_args.args[3]
            self.assertEqual(environment["BLAZE_DIST_TEST_RESULT_GATE"], str(root / "helper" / "gate"))
            self.assertEqual(environment["BLAZE_DIST_TEST_RESULT_UNIT"], h.HOST_PACKAGE)
            self.assertEqual(environment["BLAZE_DIST_TEST_RESULT_SCOPE"], "all")

    def test_distinct_hosts_are_required_independent_of_pid(self):
        template = {"namespace": "run", "commit": "commit", "tree": "tree", "blaze_sha256": "binary",
                    "compiler_seal_sha256": "seal", "workload_sha256": "fixture",
                    "triple": "aarch64-unknown-linux-gnu", "cores": 2}
        identities = [{**template, "runner": "shared-runner-name", "boot_id": f"boot-{i}"} for i in range(3)]
        h.host_same_hosts(identities)
        identities[2]["boot_id"] = identities[1]["boot_id"]
        with self.assertRaisesRegex(e.Failed, "distinct boot_id"):
            h.host_same_hosts(identities)

    def test_grants_require_shared_execution_and_disjoint_work(self):
        ranks = [host_test_held(i) for i in range(3)]
        self.assertEqual(h.host_shared_grants(ranks), (HOST_UNIT, 9, 0))
        ranks[2]["grant"]["execution"] += 1
        with self.assertRaisesRegex(e.Failed, "share one unit, execution and attempt"):
            h.host_shared_grants(ranks)
        ranks[2]["grant"]["execution"] -= 1
        ranks[2]["grant"]["work"] = ranks[1]["grant"]["work"]
        with self.assertRaisesRegex(e.Failed, "disjoint nonempty work"):
            h.host_shared_grants(ranks)

    def test_output_requires_coordinator_accepted_machine_pack(self):
        rank = host_test_held(1)
        grant = rank["grant"]
        code = bytearray(72)
        code[:8] = b"bzcode09"
        code[12:14] = b"\x04\x03"
        struct.pack_into("<I", code, 16, 4)
        code[-8:-4] = b"\xc0\x03\x5f\xd6"  # AArch64 ret
        entry = b"bzcent01" + struct.pack("<III", 9, 4, len(code)) + code
        value = bytes.fromhex(HOST_KEYS[1]) + b"\x00" + len(entry).to_bytes(8, "little") + entry
        pack = b"blzdpk\0\x01" + value
        name = hashlib.sha256(pack).hexdigest()
        rank["capture"]["command"][1]["2"] = {"3": [{"0": name}]}
        host_worker = {"identity": {"triple": "aarch64-unknown-linux-gnu"}, "rows": [{"unit": "host_proof", "unit_digest": HOST_UNIT,
                            "joined": True, "source": "executed", "execution": 9, "attempt": 0,
                            "compiled_for_work": 1, "work_compiled_keys": [HOST_KEYS[1]],
                            "tail_compiled": 0, "tail_compiled_keys": []}]}
        with tempfile.TemporaryDirectory() as directory:
            blobs = Path(directory)
            (blobs / "packs").mkdir()
            (blobs / "packs" / f"{name}.pack").write_bytes(pack)
            accepted = {**grant, "event": "result_accepted"}
            published = {HOST_KEYS[1]: (0, entry)}
            self.assertEqual(h.host_accepted_outputs(blobs, rank, host_worker, [accepted], HOST_UNIT, published), {HOST_KEYS[1]})
            with patch.dict(rank["capture"]["command"][1], {"0": HOST_UNIT}), self.assertRaisesRegex(e.Failed, "different job"):
                h.host_accepted_outputs(blobs, rank, host_worker, [accepted], HOST_UNIT, published)
            with self.assertRaisesRegex(e.Failed, "published bytes differ"):
                h.host_accepted_outputs(blobs, rank, host_worker, [accepted], HOST_UNIT, {HOST_KEYS[1]: (0, b"wrong entry")})
            accepted["seq"] += 1
            with self.assertRaisesRegex(e.Failed, "exact held batch"):
                h.host_accepted_outputs(blobs, rank, host_worker, [accepted], HOST_UNIT, published)

    def test_final_executable_has_a_byte_bound_receipt_from_both_helpers(self):
        primary = {"artifact": {"sha256": HOST_ARTIFACT}, "artifact_keys": {"artifact_sha256": HOST_ARTIFACT, "keys": HOST_KEYS[:]}}
        helpers = [{HOST_KEYS[1]}, {HOST_KEYS[2]}]
        self.assertEqual(h.host_placed_helper_keys(primary, helpers), [[HOST_KEYS[1]], [HOST_KEYS[2]]])
        primary["artifact_keys"]["keys"].pop()
        with self.assertRaisesRegex(e.Failed, "from both helpers"):
            h.host_placed_helper_keys(primary, helpers)
        primary["artifact_keys"]["artifact_sha256"] = HOST_KEYS[0]
        with self.assertRaisesRegex(e.Failed, "bind the final artifact bytes"):
            h.host_placed_helper_keys(primary, helpers)

    def test_fixture_identity_is_independent_of_checkout(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            self.assertEqual(h.host_fixture(root / "first"), h.host_fixture(root / "second"))

    def test_messages_reject_unexpected_phase(self):
        read_fd, write_fd = os.pipe()
        with os.fdopen(read_fd, "rb") as incoming, os.fdopen(write_fd, "wb") as outgoing:
            h.host_send(outgoing, {"phase": "complete"})
            with self.assertRaisesRegex(e.Failed, "expected start"):
                h.HostMessages(incoming, "helper").receive("start", time.monotonic() + 1)


if __name__ == "__main__":
    unittest.main()
