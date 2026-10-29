#!/usr/bin/env python3
"""build_release.py's artifact contracts: the portable floor, and the Nix store binding.

  python3 -B .github/scripts/test_build_release.py

These are policy tests, not a build: they write small synthetic ELF files into a temporary
directory and call the production `check_artifact`, so they can run anywhere, including a
machine with no Linux binaries and no Nix.

What they hold to:

  portable    still refuses a binary that asks for a glibc newer than RHEL 9's 2.34 -- including
              the 2.39/2.43 this repository's Nix build asks for -- and accepts 2.34 or older.
              The RHEL floor is the whole point of the tarball and must not be raised.
  nix-native  accepts a store-bound binary that asks for a newer glibc, refuses one whose loader
              is outside /nix/store, and is only usable inside a Nix build; a release job that
              passes it without one is a named error, not a silently skipped check.
"""

import contextlib
import io
import os
import re
import struct
import subprocess
import sys
import tempfile
from pathlib import Path
from typing import Optional
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parent))
import build_release as b  # noqa: E402

FAILED = []


def note(name, ok, detail=""):
    print(f"{'ok  ' if ok else 'FAIL'} {name}" + (f" -> {detail}" if detail else ""))
    if not ok:
        FAILED.append(f"{name}: {detail}")


def elf(path: Path, interp: Optional[str], wants):
    """A minimal ELF64 with one PT_INTERP and the given GLIBC_x.y strings in its body.

    Only what build_release.py reads is real: e_ident, the program header table, and the bytes
    it scans for `GLIBC_` names. Enough to exercise the checks without a toolchain.
    """
    ident = b"\x7fELF" + bytes([2, 1, 1]) + bytes(9)  # ELFCLASS64, little endian
    # One program header (PT_INTERP), the interpreter just after the table.
    phoff = 64
    phentsize, phnum = 56, 1 if interp is not None else 0
    header = ident + struct.pack("<HHIQQQIHHHHHH", 2, 62, 1, 0, phoff, 0, 0, 64, phentsize, phnum, 0, 0, 0)
    body = b""
    if interp is not None:
        data_off = phoff + phentsize
        raw = interp.encode() + b"\0"
        ph = struct.pack("<IIQQQQQQ", 3, 4, data_off, 0, 0, len(raw), len(raw), 1)
        body = ph + raw
    text = b"".join(v.encode() + b"\0" for v in wants)
    path.write_bytes(header + body + text)
    return path


def run_check(out: Path, mode: str):
    """The production check; returns (exit code, stderr-ish text)."""
    try:
        b.check_artifact(out, mode)
        return 0, ""
    except SystemExit as e:
        return 1 if (e.code not in (0, None)) else 0, str(e.code or "")


def main() -> int:
    with tempfile.TemporaryDirectory(prefix="build-release-test-") as work:
        tmp = Path(work)

        # --- portable: the RHEL 9 floor is unchanged and enforced -------------------------
        note("the portable floor is still RHEL 9's 2.34", b.GLIBC == (2, 34), f"GLIBC={b.GLIBC}")

        for asked, expect in (("GLIBC_2.34", 0), ("GLIBC_2.39", 1), ("GLIBC_2.43", 1), ("GLIBC_2.17", 0)):
            d = tmp / f"portable-{asked}"
            d.mkdir()
            elf(d / "ra-jit", "/lib64/ld-linux-x86-64.so.2", [asked])
            code, _ = run_check(d, "portable")
            note(f"portable refuses {asked}" if expect else f"portable accepts {asked}", code == expect)

        # The exact failure CI hit: several binaries, one above the floor.
        d = tmp / "portable-mixed"
        d.mkdir()
        elf(d / "blaze", "/lib64/ld-linux-x86-64.so.2", ["GLIBC_2.39"])
        elf(d / "ra-jit", "/lib64/ld-linux-x86-64.so.2", ["GLIBC_2.43"])
        code, said = run_check(d, "portable")
        note("portable reports the newest offender by name", code == 1 and "ra-jit" in said, said[:90])

        # --- nix-native: the store binding, not the RHEL floor ----------------------------
        d = tmp / "nix-newer-glibc"
        d.mkdir()
        elf(d / "ra-jit", "/nix/store/abc-glibc-2.43/lib/ld-linux-x86-64.so.2", ["GLIBC_2.43"])
        code, said = run_check(d, "nix-native")
        note("nix-native accepts a store-bound binary asking for 2.43", code == 0, said[:90])

        d = tmp / "nix-portable-loader"
        d.mkdir()
        elf(d / "ra-jit", "/lib64/ld-linux-x86-64.so.2", ["GLIBC_2.34"])
        code, said = run_check(d, "nix-native")
        note("nix-native refuses a loader outside the store", code == 1 and "outside /nix/store" in said,
             said[:90])

        for name, loader in (
            ("parent-escape", "/nix/store/../../lib64/ld-linux-x86-64.so.2"),
            ("nested-escape", "/nix/store/glibc/../../../lib64/ld-linux-x86-64.so.2"),
        ):
            d = tmp / name
            d.mkdir()
            elf(d / "ra-jit", loader, ["GLIBC_2.43"])
            code, said = run_check(d, "nix-native")
            note(f"nix-native refuses a store prefix with {name}",
                 code == 1 and "outside /nix/store" in said, said[:90])

        d = tmp / "nix-static"
        d.mkdir()
        elf(d / "ra-jit", None, ["GLIBC_2.34"])
        code, said = run_check(d, "nix-native")
        note("nix-native accepts a binary with no interpreter (static)", code == 0, said[:60])

        # A malformed ELF must never be read as static: "static" is what waives the check.
        malformed = {
            "truncated header": b"\x7fELF" + bytes([2, 1, 1]) + bytes(8),
            "bad data encoding": b"\x7fELF" + bytes([2, 2, 1]) + bytes(9) + bytes(64),
            "bad class": b"\x7fELF" + bytes([7, 1, 1]) + bytes(9) + bytes(64),
        }
        for why, blob in malformed.items():
            d = tmp / f"bad-{why.replace(' ', '-')}"
            d.mkdir()
            (d / "ra-jit").write_bytes(blob)
            code, said = run_check(d, "nix-native")
            note(f"nix-native refuses a malformed ELF ({why})", code == 1 and "must be a readable ELF" in said,
                 said[:80])

        # Program headers that run past the file, and a PT_INTERP that does.
        ident = b"\x7fELF" + bytes([2, 1, 1]) + bytes(9)
        past = ident + struct.pack("<HHIQQQIHHHHHH", 2, 62, 1, 0, 64, 0, 0, 64, 56, 40, 0, 0, 0)
        d = tmp / "phnum-past-eof"
        d.mkdir()
        (d / "ra-jit").write_bytes(past)
        code, said = run_check(d, "nix-native")
        note("nix-native refuses program headers past EOF", code == 1 and "past the file" in said, said[:80])

        interp_past = ident + struct.pack("<HHIQQQIHHHHHH", 2, 62, 1, 0, 64, 0, 0, 64, 56, 1, 0, 0, 0) \
            + struct.pack("<IIQQQQQQ", 3, 4, 4096, 0, 0, 512, 512, 1)
        d = tmp / "interp-past-eof"
        d.mkdir()
        (d / "ra-jit").write_bytes(interp_past)
        code, said = run_check(d, "nix-native")
        note("nix-native refuses a PT_INTERP past EOF", code == 1 and "past the file" in said, said[:80])

        # The class decides the minimum header: an ELF64 header is 64 bytes, so every truncation
        # from 52 (the ELF32 size) to 63 must fail by name, never as struct.error and never as
        # "static" -- static is the waiver, so a short header must not reach it.
        full64 = ident + struct.pack("<HHIQQQIHHHHHH", 2, 62, 1, 0, 64, 0, 0, 64, 56, 0, 0, 0, 0)
        note("the ELF64 fixture is a full 64-byte header", len(full64) == 64)
        bad_lengths = []
        for n in range(52, 64):
            d = tmp / f"elf64-truncated-{n}"
            d.mkdir()
            (d / "ra-jit").write_bytes(full64[:n])
            code, said = run_check(d, "nix-native")
            if not (code == 1 and "truncated ELF header" in said):
                bad_lengths.append(f"{n}:{'accepted' if code == 0 else said[:40]}")
        note("nix-native refuses every ELF64 header truncation 52..63", not bad_lengths, ",".join(bad_lengths))

        d = tmp / "elf64-full-static"
        d.mkdir()
        (d / "ra-jit").write_bytes(full64)
        code, said = run_check(d, "nix-native")
        note("nix-native accepts a full 64-byte ELF64 header with no interpreter", code == 0, said[:60])

        # ELF32 is the class whose 52-byte header the old check used for both: it must still be
        # read, and its own truncations must fail by name too.
        full32 = b"\x7fELF" + bytes([1, 1, 1]) + bytes(9) + struct.pack("<HHIIIIIHHHHHH", 2, 3, 1, 0, 52, 0,
                                                                       0, 52, 32, 0, 0, 0, 0)
        note("the ELF32 fixture is a full 52-byte header", len(full32) == 52)
        d = tmp / "elf32-full-static"
        d.mkdir()
        (d / "ra-jit").write_bytes(full32)
        code, said = run_check(d, "nix-native")
        note("nix-native accepts a full 52-byte ELF32 header with no interpreter", code == 0, said[:60])
        d = tmp / "elf32-truncated"
        d.mkdir()
        (d / "ra-jit").write_bytes(full32[:51])
        code, said = run_check(d, "nix-native")
        note("nix-native refuses a 51-byte ELF32 header", code == 1 and "truncated ELF header" in said, said[:70])

        # A PT_INTERP record must be a terminated, nonempty string inside its own bytes: a prefix
        # match on an unterminated slice would accept a loader record no loader could read.
        store = b"/nix/store/abc-glibc-2.43/lib/ld-linux-x86-64.so.2"

        def with_interp(name, raw, filesz):
            d = tmp / name
            d.mkdir()
            hdr = ident + struct.pack("<HHIQQQIHHHHHH", 2, 62, 1, 0, 64, 0, 0, 64, 56, 1, 0, 0, 0)
            ph = struct.pack("<IIQQQQQQ", 3, 4, 120, 0, 0, filesz, filesz, 1)
            (d / "ra-jit").write_bytes(hdr + ph + raw)
            return d

        code, said = run_check(with_interp("interp-terminated", store + b"\0", len(store) + 1), "nix-native")
        note("nix-native accepts a NUL-terminated store loader", code == 0, said[:60])
        code, said = run_check(with_interp("interp-unterminated", store, len(store)), "nix-native")
        note("nix-native refuses an unterminated store loader",
             code == 1 and "not NUL-terminated" in said, said[:80])
        code, said = run_check(with_interp("interp-empty-name", b"\0", 1), "nix-native")
        note("nix-native refuses a PT_INTERP that names no loader",
             code == 1 and "names no loader" in said, said[:80])

        # --- the mode is a declaration, not a way to skip a check -------------------------
        d = tmp / "unknown-mode"
        d.mkdir()
        elf(d / "ra-jit", "/nix/store/x/lib/ld.so", ["GLIBC_2.43"])
        code, said = run_check(d, "skip")
        note("an unknown mode is refused by name", code == 1 and "unknown --artifact-mode" in said, said[:80])

        # --- CLI: nix-native only inside a Nix build, portable is the default -------------
        script = Path(__file__).resolve().parent / "build_release.py"
        env = {k: v for k, v in os.environ.items() if k != "NIX_BUILD_TOP"}
        proc = subprocess.run([sys.executable, "-I", str(script), "--target", "x86_64-unknown-linux-gnu",
                               "--out", str(tmp / "cli"), "--artifact-mode", "nix-native"],
                              capture_output=True, text=True, env=env, cwd=b.ROOT)
        note("nix-native outside a Nix build is a named CLI error",
             proc.returncode == 2 and "NIX_BUILD_TOP is unset" in proc.stderr, proc.stderr.strip()[-90:])

        proc = subprocess.run([sys.executable, "-I", str(script), "--help"],
                              capture_output=True, text=True, env=env, cwd=b.ROOT)
        note("portable is the documented default",
             proc.returncode == 0 and re.search(r"portable.*default", proc.stdout, re.S) is not None)

        # The release and install workflows must keep asking for the portable contract: they
        # pass no mode, so they get the default, and no workflow may pass nix-native.
        for wf in sorted((b.ROOT / ".github/workflows").glob("*.yaml")):
            text = wf.read_text()
            if "build_release.py" not in text:
                continue
            note(f"{wf.name} does not ask for nix-native",
                 "artifact-mode nix-native" not in text)
        flake = (b.ROOT / "flake.nix").read_text()
        note("the flake's store package declares nix-native",
             "--artifact-mode nix-native" in flake)

        for counts in ([], [0], [4], [10], [0, 10]):
            out = tmp / ("bootstrap-" + str(counts))
            out.mkdir()
            executable = out / b.SERVER_BIN
            executable.write_bytes(b"new bootstrap image")
            stderr = "".join(f"linked image in 1.0s, {n} functions in it ra-jit could not compile\n"
                             for n in counts)
            built = subprocess.CompletedProcess([], 0, "", stderr)
            version = subprocess.CompletedProcess([], 0, "owned server\n", "")
            with patch.object(b, "run", return_value=built) as compile_image, \
                    patch.object(b.subprocess, "run", return_value=version) as check_version, \
                    contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(io.StringIO()):
                try:
                    b.proc_macro_server(tmp / "ra-jit", out)
                    accepted = True
                except SystemExit:
                    accepted = False
            expected = counts == [0]
            note(f"server bootstrap requires explicit zero uncompiled functions {counts}",
                 accepted == expected and executable.exists() == expected
                 and check_version.called == expected)
            note(f"server bootstrap disables all prior expansion servers {counts}",
                 compile_image.call_args.kwargs["env"]["RA_JIT_PROC_MACRO_SERVER"] == "0")

    if FAILED:
        for f in FAILED:
            print(f"FAIL {f}", file=sys.stderr)
        print(f"{len(FAILED)} failed", file=sys.stderr)
        return 1
    print("all cases passed")
    return 0


if __name__ == "__main__":
    sys.exit(main())
