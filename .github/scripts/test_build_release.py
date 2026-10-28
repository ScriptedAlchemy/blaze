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

import os
import re
import struct
import subprocess
import sys
import tempfile
from pathlib import Path
from typing import Optional

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

        d = tmp / "nix-static"
        d.mkdir()
        elf(d / "ra-jit", None, ["GLIBC_2.34"])
        code, said = run_check(d, "nix-native")
        note("nix-native accepts a binary with no interpreter (static)", code == 0, said[:60])

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

    if FAILED:
        for f in FAILED:
            print(f"FAIL {f}", file=sys.stderr)
        print(f"{len(FAILED)} failed", file=sys.stderr)
        return 1
    print("all cases passed")
    return 0


if __name__ == "__main__":
    sys.exit(main())
