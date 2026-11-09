"""Oracles over real distv2 reports, live lease events and the server's native packs.

The telemetry contract is in README.md. Missing evidence is a failure, never a skip.
"""
from __future__ import annotations

import hashlib
import json
from pathlib import Path


class Failed(Exception):
    pass


def require(ok: bool, what: str) -> None:
    if not ok:
        raise Failed(what)


def digest(value: str) -> bool:
    return isinstance(value, str) and len(value) == 64 and all(c in "0123456789abcdef" for c in value)


def jsonl(path: Path, live: bool = False) -> list[dict]:
    if not path.exists():
        return []
    raw = path.read_bytes()
    if live and not raw.endswith(b"\n"):
        raw = raw[:raw.rfind(b"\n") + 1]
    try:
        rows = [json.loads(line) for line in raw.splitlines()]
    except (ValueError, UnicodeError) as why:
        raise Failed(f"{path}: invalid JSONL evidence: {why}") from why
    require(all(isinstance(row, dict) for row in rows), f"{path}: evidence rows must be objects")
    return rows


def validate_events(events: list[dict]) -> None:
    for event in events:
        kind = event.get("event")
        require(isinstance(kind, str), "BLAZE_DIST_EVENTS: event needs its type")
        if kind in ("grant", "result_held", "result_accepted", "result_refused"):
            lease(event)
        if kind in ("grant", "result_held", "result_accepted"):
            work = event.get("work")
            require(isinstance(work, list) and all(digest(k) for k in work), f"{kind}: work must name exact WorkName digests")
            require(len(work) == len(set(work)), f"{kind}: duplicate work names")
        if kind == "grant":
            require(type(event.get("pid")) is int and event["pid"] > 0 and digest(event.get("unit_digest")),
                    "grant: needs the real daemon pid and unit_digest")
        if kind == "result_held":
            require(type(event.get("pid")) is int and event["pid"] > 0 and digest(event.get("command_sha256")),
                    "result_held: needs the real daemon pid and canonical command digest")
        if kind == "result_refused":
            require(isinstance(event.get("refusal"), str), "result_refused: needs the typed refusal")
        if kind == "done":
            require(type(event.get("execution")) is int and type(event.get("attempt")) is int
                    and digest(event.get("unit_digest")) and isinstance(event.get("outcome"), str),
                    "done: needs actual unit/execution/attempt/outcome")
        if kind == "published":
            require(digest(event.get("unit_digest")) and type(event.get("execution")) is int
                    and type(event.get("attempt")) is int and isinstance(event.get("packs"), list)
                    and all(digest(p) for p in event["packs"]), "published: needs current-attempt manifest identity")


def compiled(row: dict) -> set[str]:
    """Counters must name the actual store keys; duplicate compilation cannot hide in a sum."""
    emitted = []
    for field, count in (("work_compiled_keys", "compiled_for_work"), ("tail_compiled_keys", "tail_compiled")):
        require(field in row, f"{row.get('unit')}: report needs {field}; see distv2/README.md")
        values = row[field]
        require(isinstance(values, list) and all(digest(k) for k in values), f"{field}: expected store key digests")
        require(len(values) == row[count], f"{field}: keys disagree with {count}")
        emitted.append(values)
    work, tail = emitted
    require(len(work) == len(set(work)), f"{row['unit']}: a granted template was compiled twice")
    require(not set(work).intersection(tail), f"{row['unit']}: granted work was compiled again in the tail")
    # Distinct static instances may emit identical content keys in the local tail.
    # Keep those raw emissions in the report and its counter. Only occupancy is
    # compared across daemons and against the immutable published manifest.
    require(digest(row.get("unit_digest")), f"{row['unit']}: report needs unit_digest")
    require(type(row.get("attempt")) is int, f"{row['unit']}: report needs its final attempt")
    return set(work).union(tail)


def no_duplicate_compile(rows: list[dict]) -> None:
    by_unit: dict[str, set[str]] = {}
    require(bool(rows), "no unit reports to check for duplicate compilation")
    for row in rows:
        keys = compiled(row)
        seen = by_unit.setdefault(row["unit_digest"], set())
        require(not seen.intersection(keys), f"{row['unit']}: two daemons compiled the same store key")
        seen.update(keys)


def lease(event: dict) -> tuple:
    for name in ("execution", "attempt", "generation", "seq"):
        require(type(event.get(name)) is int and event[name] >= 0, f"{event.get('event')}: needs real lease/batch {name}")
    require(isinstance(event.get("holder"), str) and bool(event["holder"]), "lease event needs its actual holder")
    return tuple(event[name] for name in ("execution", "attempt", "holder", "generation", "seq"))


def accepted(events: list[dict], grant: dict) -> bool:
    return any(e["event"] == "result_accepted" and lease(e) == lease(grant) for e in events)


def recovered(events: list[dict], grant: dict, accepted_after: int = 0) -> bool:
    """Every outstanding name was actually committed under a replacement lease."""
    replacements = {
        lease(e): e for e in events
        if e["event"] == "grant" and e["unit_digest"] == grant["unit_digest"]
        and e["execution"] == grant["execution"]
        and (e["attempt"], e["holder"], e["generation"]) != (grant["attempt"], grant["holder"], grant["generation"])
    }
    done = set()
    for index, event in enumerate(events):
        if index >= accepted_after and event["event"] == "result_accepted" and lease(event) in replacements:
            require(set(event["work"]) <= set(replacements[lease(event)]["work"]), "accepted result names work outside its grant")
            done.update(event["work"])
    return bool(grant["work"]) and set(grant["work"]) <= done


def stale_refused(events: list[dict], grant: dict) -> bool:
    return any(
        e["event"] == "result_refused" and lease(e) == lease(grant)
        and e["refusal"] in ("DeadLease", "AttemptMoved") for e in events
    )


def pack_values(raw: bytes) -> dict[str, tuple[int, bytes]]:
    require(raw.startswith(b"blzdpk\0\x01"), "stored pack has the wrong native header")
    at, keys, values = 8, [], {}
    while at < len(raw):
        require(at + 41 <= len(raw), "stored pack ends inside a value header")
        key = raw[at:at + 32].hex()
        keys.append(key)
        kind = raw[at + 32]
        size = int.from_bytes(raw[at + 33:at + 41], "little")
        values[key] = (kind, raw[at + 41:at + 41 + size])
        at += 41 + size
        require(at <= len(raw), "stored pack ends inside a value")
    require(keys == sorted(set(keys)), "stored pack keys are not strictly sorted")
    return values


def pack_keys(raw: bytes) -> set[str]:
    return set(pack_values(raw))


def logical_outputs(blobs: Path, names) -> dict[str, tuple[int, bytes]]:
    values = {}
    for name in names:
        require(digest(name), "publication needs native pack digests")
        target = blobs / "packs" / f"{name}.pack"
        require(target.is_file(), f"accepted publication pack {name} is missing")
        raw = target.read_bytes()
        require(hashlib.sha256(raw).hexdigest() == name, f"accepted publication pack {name} is corrupt")
        for key, value in pack_values(raw).items():
            require(key not in values or values[key] == value, f"publication disagrees under store key {key}")
            values[key] = value
    return values


def pack_keys_digest(keys: set[str]) -> str:
    """Independent PackRef.keys oracle for blaze_wire's v5 canonical CBOR digest."""
    count = len(keys)
    if count < 24:
        head = bytes([0x80 | count])
    elif count < 256:
        head = b"\x98" + bytes([count])
    elif count < 65536:
        head = b"\x99" + count.to_bytes(2, "big")
    elif count < (1 << 32):
        head = b"\x9a" + count.to_bytes(4, "big")
    else:
        head = b"\x9b" + count.to_bytes(8, "big")
    # blaze_wire::digest_of_encoded_name("blaze/dist-pack-keys", encode(&Vec<Digest>)).
    h = hashlib.sha256(b"blaze/blaze/dist-pack-keys/v5\0" + head)
    for key in sorted(keys):
        h.update(b"\x58\x20")  # canonical CBOR byte string of length 32
        h.update(bytes.fromhex(key))
    return h.hexdigest()


def manifest(blobs: Path, unit: str) -> tuple[dict[str, Path], set[str]]:
    require(digest(unit), "manifest needs the real unit digest")
    path = blobs / "manifests" / unit
    require(path.is_file(), f"unit {unit}: no actual published manifest at {path}")
    packs, keys, values = {}, set(), {}
    for line in path.read_text().splitlines():
        fields = line.split()
        require(len(fields) == 3, f"{path}: malformed native manifest line {line!r}")
        name, size, key_digest = fields
        require(digest(name) and digest(key_digest) and size.isdecimal(), f"{path}: invalid pack reference")
        target = blobs / "packs" / f"{name}.pack"
        require(target.is_file(), f"{path}: referenced pack {target} is missing")
        raw = target.read_bytes()
        require(len(raw) == int(size) and hashlib.sha256(raw).hexdigest() == name, f"{target}: stored bytes disagree with manifest")
        require(name not in packs, f"{path}: duplicate pack {name}")
        entries = pack_values(raw)
        require(pack_keys_digest(set(entries)) == key_digest,
                f"{target}: manifest PackRef.keys digest disagrees with the native pack's keys")
        for key, fingerprint in entries.items():
            previous = values.get(key)
            require(previous is None or previous == fingerprint,
                    f"{path}: immutable store key {key} has different values across published packs")
            values[key] = fingerprint
        keys.update(entries)
        packs[name] = target
    return packs, keys


def publication(blobs: Path, rows: list[dict], events: list[dict]) -> None:
    for row in rows:
        keys = compiled(row)
        packs, stored = manifest(blobs, row["unit_digest"])
        require(keys <= stored, f"{row['unit']}: compiled functions missing from actual full manifest")
        if row["source"] == "executed" and row["execution"] >= 0:
            accepted_attempt = row.get("publication_attempt")
            require(type(accepted_attempt) is int and 0 <= accepted_attempt <= row["attempt"],
                    f"{row['unit']}: report needs the actual accepting attempt")
            actual = logical_outputs(blobs, packs)
            publications = [index for index, e in enumerate(events)
                if e["event"] == "published" and e["unit_digest"] == row["unit_digest"]
                and e["execution"] == row["execution"] and e["attempt"] == accepted_attempt
                and logical_outputs(blobs, e["packs"]) == actual]
            require(bool(publications), f"{row['unit']}: no accepted full output equal to the verified cache manifest")
            done = [index for index, e in enumerate(events) if e["event"] == "done"
                    and e["unit_digest"] == row["unit_digest"] and e["execution"] == row["execution"]
                    and e["attempt"] == row["attempt"] and e["outcome"] == "Passed"]
            require(any(index > min(publications) for index in done),
                    f"{row['unit']}: Done preceded the full manifest commit or was not observed")


def cached(rows: list[dict]) -> None:
    require(bool(rows), "cold build reported no units")
    for row in rows:
        require(row["source"] == "published" and row["execution"] == -1 and not row["joined"], f"{row['unit']}: cold build scheduled work")
        require(row["grants"] == 0 and row["stored_for_work"] == 0 and not compiled(row), f"{row['unit']}: cold build did work")


def held_result(gate: Path, events: list[dict], pid: int) -> dict | None:
    """Bind a product-written capture to its issued grant; readiness alone is insufficient.

    The Command JSON is Blaze's own rendering of its canonical CBOR, retained beside that
    exact CBOR and its hash. Native driver tests decode the saved CBOR and compare the real
    sent command and response. No carrier, lease or acceptance event is made by this harness.
    """
    if (gate / "error.txt").exists():
        raise Failed(f"{gate}: the product result gate failed: {(gate / 'error.txt').read_text()}")
    ready = gate / "ready.json"
    if not ready.exists():
        return None
    captured = json.loads(ready.read_text())
    require(isinstance(captured, dict) and captured.get("event") == "result_held", f"{gate}: no real result capture")
    validate_events([captured])
    require(captured["pid"] == pid, f"{gate}: the capture names another daemon")
    raw = (gate / "result.cbor").read_bytes()
    require(0 < len(raw) <= 16 << 20 and hashlib.sha256(raw).hexdigest() == captured["command_sha256"],
            f"{gate}: captured CBOR disagrees with its product-written digest")
    command = captured.get("command")
    require(isinstance(command, list) and len(command) == 2 and command[0] == 7 and raw.startswith(b"\x82\x07"),
            f"{gate}: capture must be an actual Command::Result")
    try:
        submission = command[1]
        batch = submission["2"]
        issued = batch["0"]
        actual = {"execution": issued["0"], "attempt": issued["1"], "holder": issued["2"],
                  "generation": issued["3"], "seq": batch["1"]}
        work = [item["0"] for item in batch["2"]]
        valid = (digest(submission["0"]) and submission["1"] == issued["2"]
                 and batch["4"] == [] and batch["5"] == [])
    except (KeyError, TypeError, IndexError) as why:
        raise Failed(f"{gate}: malformed actual Result view") from why
    require(valid and lease(actual) == lease(captured) and work == captured["work"] and bool(work),
            f"{gate}: captured identity/items differ from its real Result carrier")
    matching = [e for e in events if e["event"] == "grant" and e["pid"] == pid and lease(e) == lease(captured)]
    require(bool(matching) and all(e["work"] == work for e in matching), f"{gate}: capture has no matching actual issued grant")
    if not any(e["event"] == "result_held" and e["pid"] == pid and e["work"] == work and lease(e) == lease(captured)
               and e["command_sha256"] == captured["command_sha256"] for e in events):
        return None  # ready.json is named immediately before the real append becomes visible.
    grant = {**matching[-1], "capture_sha256": captured["command_sha256"]}
    require(not accepted(events, grant), f"{gate}: the gate did not hold an uncommitted result")
    return grant


def gate_answer(gate: Path, grant: dict, *, refused: bool) -> bool:
    """Require the normal Client's saved answer as well as the coordinator's event."""
    if (gate / "error.txt").exists():
        raise Failed(f"{gate}: the product result gate failed: {(gate / 'error.txt').read_text()}")
    path = gate / "answer.json"
    if not path.exists():
        return False
    require(hashlib.sha256((gate / "result.cbor").read_bytes()).hexdigest() == grant["capture_sha256"],
            f"{gate}: the captured result changed before its answer")
    view = json.loads(path.read_text())
    raw = (gate / "answer.cbor").read_bytes()
    require(isinstance(view, dict) and digest(view.get("sha256")) and hashlib.sha256(raw).hexdigest() == view["sha256"],
            f"{gate}: answer CBOR disagrees with its product-written digest")
    answer = view.get("answer")
    tag = 9 if refused else 5
    require(isinstance(answer, list) and len(answer) == 2 and answer[0] == tag and raw.startswith(bytes((0x82, tag))),
            f"{gate}: expected the normal Client's {'refusal' if refused else 'accepted Result'} answer")
    if refused:
        error = answer[1]
        require(isinstance(error, dict) and error.get("0") == 6 and isinstance(error.get("1"), str)
                and error["1"].split(": ", 1)[0] in ("dead-lease", "attempt-moved"),
                f"{gate}: the actual answer must carry DeadLease or AttemptMoved")
    return True


def release_fold(rows: list[dict], events: list[dict]) -> None:
    """Planned kinds count only when joined to raw emissions and accepted real grants."""
    holders: dict[str, dict[tuple, set[int]]] = {"release_keys": {}, "folded_keys": {}}
    emitted = [event for event in events if event.get("event") == "backend_work"]
    require(bool(emitted), "release/fold acceptance needs backend_work emission evidence")
    for event in emitted:
        actual = event.get("compiled_keys")
        require(isinstance(actual, list) and all(digest(key) for key in actual), "backend_work needs raw emitted store keys")
        matching = [grant for grant in events if grant.get("event") == "grant"
                    and grant["pid"] == event.get("pid") and grant["unit_digest"] == event.get("unit_digest")
                    and sorted(grant["work"]) == sorted(event.get("work", [])) and accepted(events, grant)]
        if not matching:
            continue  # A failed/stale attempt cannot supply successful emission evidence.
        for grant in matching:
            reports = [row for row in rows if row["unit_digest"] == grant["unit_digest"]
                       and row["execution"] == grant["execution"] and row["attempt"] == grant["attempt"]]
            reported = {key for row in reports for key in row["work_compiled_keys"]}
            require(set(actual) <= reported, "backend emission was absent from final work reports")
            identity = (grant["unit_digest"], grant["execution"], grant["attempt"])
            for kind in holders:
                keys = event.get(kind)
                require(isinstance(keys, list) and all(digest(key) for key in keys), f"backend_work needs {kind}")
                require(set(keys) <= set(actual), f"{kind} includes an uncompiled planned key")
                if keys:
                    holders[kind].setdefault(identity, set()).add(grant["pid"])
    for kind, units in holders.items():
        require(any(len(pids) >= 2 for pids in units.values()),
                f"two real holders must emit {kind} in one successful unit/execution/attempt")
