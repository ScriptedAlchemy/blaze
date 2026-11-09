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
        if kind in ("grant", "result_accepted", "result_refused"):
            lease(event)
        if kind in ("grant", "result_accepted"):
            work = event.get("work")
            require(isinstance(work, list) and all(digest(k) for k in work), f"{kind}: work must name exact WorkName digests")
            require(len(work) == len(set(work)), f"{kind}: duplicate work names")
        if kind == "grant":
            require(type(event.get("pid")) is int and event["pid"] > 0 and digest(event.get("unit_digest")),
                    "grant: needs the real daemon pid and unit_digest")
        if kind == "result_refused":
            require(isinstance(event.get("refusal"), str), "result_refused: needs the typed refusal")
        if kind == "done":
            require(type(event.get("execution")) is int and type(event.get("attempt")) is int
                    and isinstance(event.get("outcome"), str), "done: needs actual execution/attempt/outcome")
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
    """Every outstanding name was committed under a replacement grant before acceptance."""
    replacements: dict[tuple, set[str]] = {}
    done = set()
    for index, event in enumerate(events):
        if (event["event"] == "grant" and event["unit_digest"] == grant["unit_digest"]
                and event["execution"] == grant["execution"]
                and (event["attempt"], event["holder"], event["generation"])
                    != (grant["attempt"], grant["holder"], grant["generation"])):
            replacements[lease(event)] = set(event["work"])
        elif index >= accepted_after and event["event"] == "result_accepted":
            allowed = replacements.get(lease(event))
            if allowed is not None:
                require(set(event["work"]) <= allowed, "accepted result names work outside its grant")
                done.update(event["work"])
    return bool(grant["work"]) and set(grant["work"]) <= done


def stale_refused(events: list[dict], grant: dict, refused_after: int = 0) -> bool:
    """Only a refusal observed after the stale holder resumes proves fencing."""
    return any(
        index >= refused_after and event["event"] == "result_refused"
        and lease(event) == lease(grant)
        and event["refusal"] in ("DeadLease", "AttemptMoved")
        for index, event in enumerate(events)
    )


def pack_entries(raw: bytes) -> dict[str, bytes]:
    """Parse the native pack independently, fingerprinting kind and exact value bytes."""
    require(raw.startswith(b"blzdpk\0\x01"), "stored pack has the wrong native header")
    at, entries = 8, {}
    previous = ""
    while at < len(raw):
        require(at + 41 <= len(raw), "stored pack ends inside a value header")
        key = raw[at:at + 32].hex()
        require(key > previous, "stored pack keys are not strictly sorted")
        size = int.from_bytes(raw[at + 33:at + 41], "little")
        end = at + 41 + size
        require(end <= len(raw), "stored pack ends inside a value")
        # Include kind and length; a content collision under a store key is forbidden.
        entries[key] = hashlib.sha256(raw[at + 32:end]).digest()
        at, previous = end, key
    return entries


def pack_keys(raw: bytes) -> set[str]:
    return set(pack_entries(raw))


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
        entries = pack_entries(raw)
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
            require("attempt" in row, f"{row['unit']}: report needs its final attempt")
            publications = [index for index, e in enumerate(events)
                if e["event"] == "published" and e["unit_digest"] == row["unit_digest"]
                and e["execution"] == row["execution"] and e["attempt"] == row["attempt"]
                and set(e["packs"]) == set(packs)]
            require(bool(publications), f"{row['unit']}: no current-attempt commit of the actual full manifest")
            done = [index for index, e in enumerate(events) if e["event"] == "done"
                    and e["execution"] == row["execution"] and e["attempt"] == row["attempt"] and e["outcome"] == "Passed"]
            require(bool(done) and min(done) > min(publications), f"{row['unit']}: Done preceded the full manifest commit or was not observed")


def cached(rows: list[dict]) -> None:
    require(bool(rows), "cold build reported no units")
    for row in rows:
        require(row["source"] == "published" and row["execution"] == -1 and not row["joined"], f"{row['unit']}: cold build scheduled work")
        require(row["grants"] == 0 and row["stored_for_work"] == 0 and not compiled(row), f"{row['unit']}: cold build did work")
