#!/usr/bin/env python3
"""Extra, incomplete /proc diagnostics around an unchanged execution observer.

    python3 protocol_process_maps.py --observer /abs/exec_observer.py \
        --input /abs/original-input.json --output /abs/new-diagnostics-dir

The observer receives the input pathname unchanged, with inherited cwd, environment,
stdin/stdout/stderr and limits. Its exact exit status is preserved. This wrapper does
not attach, trace, signal guest processes, warm code, or change core/sysctl settings.
Only trace-file IDs whose /proc/ID/exe matches the input Blaze's sibling launcher
are sampled. IDs include threads: each snapshot is keyed by ID and kernel start time.
Snapshots are diagnostic samples, never evidence of complete process observation.
Copy only this diagnostic wrapper before checking out an immutable source, then run
that selected source's unchanged observer and its adjacent child helper.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import resource
import signal
import stat
import struct
import subprocess
import sys
import time


MAX_BYTES = 64 * 1024 * 1024
MAX_IDS = 4096
MAX_TEXT = 1024 * 1024
MAX_NATIVE_BYTES = 64 * 1024 * 1024
POLL_SECONDS = 0.15
TRACE_NAME = re.compile(r"exec\.trace\.([1-9][0-9]*)\Z")
LIBC_NAME = re.compile(r"libc(?:\.so(?:\.[0-9]+)*|-[0-9.]+\.so)\Z")
DIR_FLAGS = os.O_RDONLY | os.O_DIRECTORY | os.O_CLOEXEC


def stat_identity(raw: bytes) -> tuple[int, int]:
    """The comm field may contain spaces and closing parentheses (proc_pid_stat)."""
    left, right = raw.find(b"("), raw.rfind(b")")
    if left < 1 or right < left:
        raise ValueError("malformed proc stat comm")
    fields = raw[right + 1:].split()
    if len(fields) < 20:
        raise ValueError("short proc stat")
    pid, started = int(raw[:left].strip()), int(fields[19])
    if pid < 1 or started < 0:
        raise ValueError("invalid proc stat identity")
    return pid, started


def read_at(directory: int, name: str, limit: int = MAX_TEXT) -> bytes:
    fd = os.open(name, os.O_RDONLY | os.O_CLOEXEC | os.O_NONBLOCK, dir_fd=directory)
    try:
        chunks, size = [], 0
        while True:
            chunk = os.read(fd, min(65536, limit + 1 - size))
            if not chunk:
                return b"".join(chunks)
            size += len(chunk)
            if size > limit:
                raise ValueError(f"{name} exceeds read bound")
            chunks.append(chunk)
    finally:
        os.close(fd)


def mapped_files(raw: bytes) -> list[tuple[str, str, int]]:
    files = []
    for line in raw.decode("utf-8", "surrogateescape").splitlines():
        fields = line.split(None, 5)
        if len(fields) == 6 and fields[5].startswith("/"):
            files.append((fields[5], fields[3], int(fields[4])))
    return files


def has_libc(files: list[tuple[str, str, int]]) -> bool:
    return any(LIBC_NAME.fullmatch(Path(path).name) is not None for path, _, _ in files)


def elf_build_id(fd: int, size: int) -> str | None:
    """Bounded reads of ELF program notes; no external program or library is loaded."""
    head = os.pread(fd, 64, 0)
    if len(head) < 52 or head[:4] != b"\x7fELF" or head[4] not in (1, 2) or head[5] not in (1, 2):
        return None
    endian = "<" if head[5] == 1 else ">"
    wide = head[4] == 2
    if wide and len(head) < 64:
        return None
    phoff = struct.unpack_from(endian + ("Q" if wide else "I"), head, 32 if wide else 28)[0]
    entsize, count = struct.unpack_from(endian + "HH", head, 54 if wide else 42)
    if count > 512 or entsize < (56 if wide else 32) or entsize > 256 or phoff + count * entsize > size:
        return None
    note_budget = MAX_TEXT
    for index in range(count):
        entry = os.pread(fd, entsize, phoff + index * entsize)
        if len(entry) != entsize or struct.unpack_from(endian + "I", entry)[0] != 4:
            continue
        off = struct.unpack_from(endian + ("Q" if wide else "I"), entry, 8 if wide else 4)[0]
        length = struct.unpack_from(endian + ("Q" if wide else "I"), entry, 32 if wide else 16)[0]
        if length > note_budget or off + length > size:
            continue
        note_budget -= length
        notes = os.pread(fd, length, off)
        at = 0
        while at + 12 <= len(notes):
            names, desc, kind = struct.unpack_from(endian + "III", notes, at)
            start = at + 12
            value = start + ((names + 3) & ~3)
            following = value + ((desc + 3) & ~3)
            if following > len(notes):
                break
            if kind == 3 and notes[start:start + names] == b"GNU\0" and 0 < desc <= 64:
                return notes[value:value + desc].hex()
            at = following
    return None


class Collector:
    def __init__(self, trace_dir: Path, launcher: Path, output: Path,
                 byte_limit: int = MAX_BYTES, id_limit: int = MAX_IDS):
        self.trace_dir, self.launcher, self.output = trace_dir, str(launcher), output
        self.byte_limit, self.id_limit = byte_limit, id_limit
        self.bytes = 0
        self.native_bytes = 0
        self.seen: dict[int, tuple[int, int, int]] = {}
        self.pending: set[int] = set()
        self.captured: set[tuple[int, int]] = set()
        self.maps_written: set[str] = set()
        self.libraries: dict[tuple[int, int, int, int], dict[str, object]] = {}
        self.gaps: dict[str, int] = {}
        self.examples: list[dict[str, object]] = []
        self.caps: set[str] = set()
        self.disabled = False
        self.output.mkdir(mode=0o700)

    def gap(self, kind: str, detail: str = "") -> None:
        self.gaps[kind] = self.gaps.get(kind, 0) + 1
        if len(self.examples) < 32:
            self.examples.append({"kind": kind, "detail": detail[:256]})

    def write(self, name: str, data: bytes) -> bool:
        # Reserve 16 KiB for the final receipt, including explicit sampling limitations.
        if self.bytes + len(data) > self.byte_limit - 16384:
            self.caps.add("output_bytes")
            self.disabled = True
            return False
        path = self.output / name
        path.parent.mkdir(parents=True, exist_ok=True)
        with path.open("xb") as stream:
            stream.write(data)
        self.bytes += len(data)
        return True

    def native(self, files: list[tuple[str, str, int]]) -> None:
        for path, dev, inode in files:
            name = Path(path).name
            if not (LIBC_NAME.fullmatch(name) or name.startswith(("ld-linux", "ld-musl", "libgcc_s.so", "libpthread.so"))):
                continue
            fd = None
            try:
                # Nonblocking open plus fstat avoids blocking on an unexpected special file.
                fd = os.open(path, os.O_RDONLY | os.O_CLOEXEC | os.O_NONBLOCK)
                before = os.fstat(fd)
                major, minor = (int(part, 16) for part in dev.split(":"))
                if not stat.S_ISREG(before.st_mode) or (os.major(before.st_dev), os.minor(before.st_dev), before.st_ino) != (major, minor, inode):
                    raise ValueError("mapped device/inode does not match opened regular file")
                key = (before.st_dev, before.st_ino, before.st_size, before.st_mtime_ns)
                if key in self.libraries:
                    continue
                if len(self.libraries) >= 64 or self.native_bytes + before.st_size > MAX_NATIVE_BYTES:
                    self.caps.add("native_hashing")
                    continue
                hashed = hashlib.sha256()
                remaining = before.st_size
                while remaining:
                    data = os.read(fd, min(65536, remaining))
                    if not data:
                        raise ValueError("native file shortened while hashing")
                    self.native_bytes += len(data)
                    remaining -= len(data)
                    hashed.update(data)
                build_id = elf_build_id(fd, before.st_size)
                after = os.fstat(fd)
                if (before.st_size, before.st_mtime_ns, before.st_ctime_ns) != (after.st_size, after.st_mtime_ns, after.st_ctime_ns):
                    raise ValueError("native file changed while hashing")
                record = {"path": path, "device": dev, "inode": inode, "size": before.st_size,
                          "sha256": hashed.hexdigest(), "gnu_build_id": build_id}
                self.libraries[key] = record
                self.write(f"native/{hashed.hexdigest()}.json", json_bytes(record))
            except (OSError, ValueError, struct.error) as error:
                self.gap("native_identity_unavailable", f"{path}: {error}")
            finally:
                if fd is not None:
                    os.close(fd)

    def sample(self, pid: int) -> bool:
        """False means this live matching thread has not exposed libc yet."""
        proc = None
        try:
            proc = os.open(f"/proc/{pid}", DIR_FLAGS)
            raw_stat = read_at(proc, "stat", 16384)
            identity = stat_identity(raw_stat)
            if identity[0] != pid:
                raise ValueError("proc stat does not name traced ID")
            if identity in self.captured:
                return True
            if len(self.captured) >= self.id_limit:
                self.caps.add("captured_identities")
                return True
            if os.readlink("exe", dir_fd=proc) != self.launcher:
                self.gap("non_launcher_id", str(pid))
                return True
            maps = read_at(proc, "maps")
            files = mapped_files(maps)
            if not has_libc(files):
                return False
            comm = read_at(proc, f"task/{pid}/comm", 4096)
            status = read_at(proc, "status", 65536)
            tgid_line = next((line for line in status.splitlines() if line.startswith(b"Tgid:")), b"")
            tgid = int(tgid_line.split()[1])
            # Recheck the same opened proc directory before accepting the snapshot.
            # An exit/reuse cannot redirect these relative reads to a new PID directory.
            if stat_identity(read_at(proc, "stat", 16384)) != identity or os.readlink("exe", dir_fd=proc) != self.launcher:
                raise ValueError("process identity changed during snapshot")
            digest = hashlib.sha256(maps).hexdigest()
            base = f"threads/{pid}-{identity[1]}"
            record = {"trace_id": pid, "tgid": tgid, "starttime_ticks": identity[1],
                      "sampled_unix_ns": time.time_ns(), "exe": self.launcher,
                      "maps_sha256": digest, "maps": f"maps/{digest}.txt"}
            if digest not in self.maps_written:
                if not self.write(f"maps/{digest}.txt", maps):
                    return True
                self.maps_written.add(digest)
            for suffix, data in (("stat.txt", raw_stat), ("comm.txt", comm), ("snapshot.json", json_bytes(record))):
                if not self.write(f"{base}/{suffix}", data):
                    return True
            self.captured.add(identity)
            self.native(files)
            return True
        except (OSError, ValueError, IndexError) as error:
            self.gap("snapshot_unavailable", f"{pid}: {error}")
            return True
        finally:
            if proc is not None:
                os.close(proc)

    def poll(self) -> None:
        if self.disabled:
            return
        try:
            with os.scandir(self.trace_dir) as entries:
                for entry in entries:
                    match = TRACE_NAME.fullmatch(entry.name)
                    if match is None or not entry.is_file(follow_symlinks=False):
                        continue
                    pid = int(match.group(1))
                    if pid not in self.seen and len(self.seen) >= self.id_limit:
                        self.caps.add("trace_ids")
                        continue
                    meta = entry.stat(follow_symlinks=False)
                    signature = (meta.st_ino, meta.st_mtime_ns, meta.st_size)
                    if self.seen.get(pid) != signature:
                        self.pending.add(pid)
                        self.seen[pid] = signature
        except FileNotFoundError:
            return  # The unchanged observer has not created its output directory yet.
        for pid in tuple(self.pending):
            if self.disabled:
                break
            if self.sample(pid):
                self.pending.discard(pid)

    def finish(self, metadata: dict[str, object], returncode: int) -> None:
        report = {**metadata, "observer_returncode": returncode,
                  "completeness_claim": False,
                  "limitations": "Polling can miss short-lived processes, threads and later mapping changes. Diagnostic gaps do not alter the observer result.",
                  "trace_ids_seen": len(self.seen), "thread_snapshots": len(self.captured),
                  "pending_without_libc": sorted(self.pending)[:64], "pending_count": len(self.pending),
                  "gap_counts": self.gaps, "gap_examples": self.examples,
                  "caps_reached": sorted(self.caps), "snapshot_bytes": self.bytes,
                  "native_bytes_hashed": self.native_bytes, "native_identities": len(self.libraries),
                  "finished_unix_ns": time.time_ns()}
        data = json_bytes(report)
        if len(data) > 16384:
            report["gap_examples"] = []
            data = json_bytes(report)
        if self.bytes + len(data) > self.byte_limit:
            raise ValueError("diagnostic receipt exceeds reserved output bound")
        with (self.output / "diagnostic.json").open("xb") as stream:
            stream.write(data)


def json_bytes(value: object) -> bytes:
    return (json.dumps(value, indent=2, sort_keys=True) + "\n").encode()


def preserve_status(returncode: int) -> int:
    if returncode < 0:
        number = -returncode
        if number not in (signal.SIGKILL, signal.SIGSTOP):
            signal.signal(number, signal.SIG_DFL)
        os.kill(os.getpid(), number)
        os._exit(128 + number)
    return returncode


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--observer", required=True)
    parser.add_argument("--input", required=True)
    parser.add_argument("--output", required=True)
    args = parser.parse_args()
    for value in (args.observer, args.input, args.output):
        if not Path(value).is_absolute():
            parser.error("observer, input and output must be absolute paths")
    with Path(args.input).open("rb") as stream:
        input_bytes = stream.read(MAX_TEXT + 1)
    if len(input_bytes) > MAX_TEXT:
        parser.error("observer input exceeds 1 MiB")
    given = json.loads(input_bytes)
    blaze = Path(given["blaze"]).resolve(strict=True)
    launcher = (blaze.parent / "blaze-launcher").resolve(strict=True)
    trace_dir = Path(given["out"]).resolve()
    output = Path(args.output).resolve()
    for path in (trace_dir, Path(given["home"]).resolve()):
        if output == path or output in path.parents or path in output.parents:
            parser.error("diagnostics must be separate from observer HOME and output")
    collector = Collector(trace_dir, launcher, output)
    argv = [sys.executable, args.observer, args.input]
    metadata: dict[str, object] = {"schema": 1, "observer_argv": argv,
        "input_sha256": hashlib.sha256(input_bytes).hexdigest(), "expected_launcher": str(launcher),
        "started_unix_ns": time.time_ns(), "poll_seconds": POLL_SECONDS,
        "bounds": {"output_bytes": MAX_BYTES, "trace_ids": MAX_IDS, "native_hash_bytes": MAX_NATIVE_BYTES},
        "rlimit_core": list(resource.getrlimit(resource.RLIMIT_CORE))}
    try:
        metadata["core_pattern"] = Path("/proc/sys/kernel/core_pattern").read_text()[:4096]
    except OSError as error:
        collector.gap("core_pattern_unavailable", str(error))
    process: subprocess.Popen[bytes] | None = None
    pending_signals: list[int] = []

    def interrupted(number: int, _frame: object) -> None:
        if process is None:
            pending_signals.append(number)
        elif process.poll() is None:
            try:
                process.send_signal(number)  # Only our still-owned observer, never guest PIDs.
            except ProcessLookupError:
                pass

    previous = {number: signal.signal(number, interrupted) for number in (signal.SIGINT, signal.SIGTERM)}
    try:
        process = subprocess.Popen(argv)
        for number in pending_signals:
            interrupted(number, None)
        while process.poll() is None:
            try:
                collector.poll()
            except Exception as error:
                collector.gap("collector_disabled", repr(error))
                collector.disabled = True
            try:
                process.wait(timeout=POLL_SECONDS)
            except subprocess.TimeoutExpired:
                pass
        status = process.wait()
        try:
            collector.poll()
        except Exception as error:
            collector.gap("final_poll_failed", repr(error))
        try:
            collector.finish(metadata, status)
        except Exception as error:
            print(f"protocol_process_maps: diagnostic receipt failed: {error}", file=sys.stderr)
    finally:
        for number, handler in previous.items():
            signal.signal(number, handler)
    return preserve_status(status)


if __name__ == "__main__":
    sys.exit(main())
